//! Native macOS utun backend for the CLI client (see docs/macos.md).
//!
//! The device is opened through the `tun` crate (the same version the server
//! uses), which on macOS talks to the kernel utun driver and transparently
//! strips/prepends the 4-byte address-family header, so the dispatcher sees
//! plain IP packets. Interface address and routes are applied later, when the
//! server sends TUNCONF, through `UtunControl` (external `ifconfig`/`route`
//! commands, so no borrow of the device is needed).

use anyhow::{Context, Result, bail};
use std::net::Ipv4Addr;
use std::process::Command;
use std::sync::Mutex;

/// TUN device owned by the dispatcher IO loop.
pub struct UtunDevice {
    device: ::tun::Device,
    name: String,
}

impl UtunDevice {
    pub fn create(mtu: u16) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("создание utun-интерфейса требует права root — запустите клиент через sudo");
        }
        let mut config = ::tun::Configuration::default();
        config.mtu(mtu).up();
        let mut device = ::tun::create(&config).context("создание utun-интерфейса (нужен root)")?;
        device
            .set_nonblock()
            .context("перевод utun-интерфейса в nonblocking")?;
        let name =
            ::tun::AbstractDevice::tun_name(&device).context("получение имени utun-интерфейса")?;
        crate::log_error!("[UTUN] Интерфейс {name} создан (MTU {mtu})");
        Ok(Self { device, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl std::os::fd::AsRawFd for UtunDevice {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.device.as_raw_fd()
    }
}

impl std::io::Read for &UtunDevice {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.device.recv(buf)
    }
}

impl std::io::Write for &UtunDevice {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.device.send(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Routes added by `apply_tunnel_config`, tracked for best-effort cleanup.
struct AppliedRoutes {
    /// `route` invocations that were applied successfully (args after `route -n`).
    adds: Vec<Vec<String>>,
}

/// Control handle for an already-open utun interface: applies the TUNCONF
/// address/routes and cleans them up on shutdown.
pub struct UtunControl {
    name: String,
    mtu: u16,
    peer: Ipv4Addr,
    full_tunnel: bool,
    endpoints: Vec<Ipv4Addr>,
    applied: Mutex<Option<AppliedRoutes>>,
}

impl UtunControl {
    pub fn new(
        name: String,
        mtu: u16,
        peer: Ipv4Addr,
        full_tunnel: bool,
        endpoints: Vec<Ipv4Addr>,
    ) -> Self {
        Self {
            name,
            mtu,
            peer,
            full_tunnel,
            endpoints,
            applied: Mutex::new(None),
        }
    }

    /// Called when the server delivers TUNCONF (client IP / DNS). Idempotent:
    /// only the first TUNCONF is applied. On any failure everything added so
    /// far is rolled back, so the system network is left untouched.
    pub fn apply_tunnel_config(&self, client_ip: &str, dns: &str) -> Result<()> {
        let client: Ipv4Addr = client_ip
            .parse()
            .with_context(|| format!("TUNCONF: некорректный клиентский IP {client_ip:?}"))?;
        let mut applied = self.applied.lock().unwrap();
        if applied.is_some() {
            return Ok(());
        }
        let mut adds: Vec<Vec<String>> = Vec::new();
        let result = self.apply_inner(client, dns, &mut adds);
        match result {
            Ok(()) => {
                crate::log_error!("[UTUN] Адрес {client} и маршруты применены к {}", self.name);
                *applied = Some(AppliedRoutes { adds });
                Ok(())
            }
            Err(error) => {
                for args in adds.iter().rev() {
                    let _ = run_route(&delete_args(args));
                }
                Err(error)
            }
        }
    }

    fn apply_inner(&self, client: Ipv4Addr, dns: &str, adds: &mut Vec<Vec<String>>) -> Result<()> {
        let ifconfig = vec![
            self.name.clone(),
            "inet".to_owned(),
            client.to_string(),
            self.peer.to_string(),
            "mtu".to_owned(),
            self.mtu.to_string(),
            "up".to_owned(),
        ];
        run_command("/sbin/ifconfig", &ifconfig).context("настройка адреса utun")?;

        let octets = client.octets();
        let subnet = format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]);
        let subnet_route = vec![
            "add".to_owned(),
            "-net".to_owned(),
            subnet.clone(),
            "-interface".to_owned(),
            self.name.clone(),
        ];
        run_route(&subnet_route).context("добавление маршрута в подсеть туннеля")?;
        adds.push(subnet_route);

        if self.full_tunnel {
            let gateway = default_gateway()
                .context("полный туннель: не удалось определить шлюз по умолчанию")?;
            // Protect transport endpoints and local ranges from being routed
            // into our own tunnel (more specific routes win over 0.0.0.0/1).
            for ip in &self.endpoints {
                let host_route = vec![
                    "add".to_owned(),
                    "-host".to_owned(),
                    ip.to_string(),
                    gateway.to_string(),
                ];
                run_route(&host_route)
                    .with_context(|| format!("защита маршрута до endpoint {ip}"))?;
                adds.push(host_route);
            }
            for net in [
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "169.254.0.0/16",
            ] {
                let net_route = vec![
                    "add".to_owned(),
                    "-net".to_owned(),
                    net.to_owned(),
                    gateway.to_string(),
                ];
                run_route(&net_route)
                    .with_context(|| format!("исключение локальной сети {net}"))?;
                adds.push(net_route);
            }
            for net in ["0.0.0.0/1", "128.0.0.0/1"] {
                let full_route = vec![
                    "add".to_owned(),
                    "-net".to_owned(),
                    net.to_owned(),
                    "-interface".to_owned(),
                    self.name.clone(),
                ];
                run_route(&full_route).with_context(|| format!("маршрут {net} в туннель"))?;
                adds.push(full_route);
            }
            crate::log_error!(
                "[UTUN] Полный туннель включён: endpoint'ы и LAN исключены через шлюз {gateway}"
            );
        }

        if !dns.is_empty() {
            crate::log_error!(
                "[UTUN] DNS от сервера: {dns} (системный DNS не изменяется; см. docs/macos.md)"
            );
        }
        Ok(())
    }
}

impl Drop for UtunControl {
    fn drop(&mut self) {
        // A poisoned lock must not panic inside drop: skip the best-effort
        // cleanup instead of aborting the process during unwinding.
        let applied = self
            .applied
            .lock()
            .map(|mut guard| guard.take())
            .unwrap_or(None);
        if let Some(applied) = applied {
            // The utun interface itself is destroyed (and interface routes with
            // it) once the dispatcher closes the device; explicit deletes are
            // best-effort and matter for gateway-based host/network routes.
            for args in applied.adds.iter().rev() {
                if let Err(error) = run_route(&delete_args(args)) {
                    crate::log_error!(
                        "[UTUN] Не удалось удалить маршрут ({}): {error}",
                        delete_args(args).join(" ")
                    );
                }
            }
            crate::log_error!("[UTUN] Маршруты очищены");
        }
    }
}

/// Mirrors a `route -n add ...` invocation into the matching `delete`.
fn delete_args(add: &[String]) -> Vec<String> {
    let mut args = add.to_vec();
    if let Some(first) = args.first_mut() {
        *first = "delete".to_owned();
    }
    args
}

fn run_route(args: &[String]) -> Result<()> {
    let mut full = vec!["-n".to_owned()];
    full.extend_from_slice(args);
    run_command("/sbin/route", &full)
}

fn run_command(program: &str, args: &[String]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("запуск {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn default_gateway() -> Result<Ipv4Addr> {
    let output = Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .context("запуск route get default")?;
    if !output.status.success() {
        bail!(
            "route -n get default: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("gateway:") {
            let gateway: Ipv4Addr = rest
                .split_whitespace()
                .next()
                .context("не удалось разобрать шлюз по умолчанию")?
                .parse()
                .context("некорректный шлюз по умолчанию")?;
            return Ok(gateway);
        }
    }
    bail!("в выводе `route -n get default` нет gateway")
}

#[cfg(test)]
mod tests {
    use super::delete_args;

    #[test]
    fn delete_args_mirrors_add() {
        let add = vec![
            "add".to_owned(),
            "-net".to_owned(),
            "10.66.67.0/24".to_owned(),
            "-interface".to_owned(),
            "utun5".to_owned(),
        ];
        assert_eq!(
            delete_args(&add),
            vec!["delete", "-net", "10.66.67.0/24", "-interface", "utun5"]
        );
    }
}
