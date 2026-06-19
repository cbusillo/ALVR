use alvr_common::anyhow::Result;

#[cfg(not(target_vendor = "apple"))]
use alvr_common::anyhow::bail;

#[cfg(not(target_vendor = "apple"))]
use mdns_sd::{ServiceDaemon, ServiceInfo};

#[cfg(target_vendor = "apple")]
pub struct AnnouncerSocket;

#[cfg(not(target_vendor = "apple"))]
pub struct AnnouncerSocket {
    hostname: String,
    daemon: ServiceDaemon,
}

#[cfg(target_vendor = "apple")]
impl AnnouncerSocket {
    pub fn new(_hostname: &str) -> Result<Self> {
        Ok(Self)
    }

    pub fn announce(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(target_vendor = "apple"))]
impl AnnouncerSocket {
    pub fn new(hostname: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;

        Ok(Self {
            daemon,
            hostname: hostname.to_owned(),
        })
    }

    pub fn announce(&self) -> Result<()> {
        let local_ip = alvr_system_info::local_ip();
        if local_ip.is_unspecified() {
            bail!("IP is unspecified");
        }

        let service_name = format!("alvr{}", rand::random::<u16>());
        self.daemon.register(ServiceInfo::new(
            alvr_sockets::MDNS_SERVICE_TYPE,
            &service_name,
            &self.hostname,
            local_ip,
            5353,
            &[(
                alvr_sockets::MDNS_PROTOCOL_KEY,
                alvr_common::protocol_id().as_str(),
            )][..],
        )?)?;

        Ok(())
    }
}
