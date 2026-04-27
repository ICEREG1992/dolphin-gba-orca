use serde::Serialize;

pub const HTTP_PORT: u16 = 8080;
pub const MEDIAMTX_RTMP_PORT: u16 = 1935;
pub const MEDIAMTX_WEBRTC_PORT: u16 = 8889;

#[derive(Serialize, Clone)]
pub struct NetInterface {
    pub name: String,
    pub ip: String,
    pub score: i32,
}

#[derive(Serialize, Clone)]
pub struct ServerInfo {
    pub interfaces: Vec<NetInterface>,
    pub port: u16,
    pub webrtc_port: u16,
}

fn score_interface(name: &str, ip: &std::net::Ipv4Addr) -> i32 {
    let lower = name.to_lowercase();
    let mut score = 0;

    for kw in [
        "vethernet", "vmware", "virtualbox", "wsl", "hyper-v",
        "loopback", "bluetooth", "tap", "tun", "docker", "tailscale", "zerotier",
    ] {
        if lower.contains(kw) { score -= 100; }
    }

    for kw in ["wi-fi", "wifi", "ethernet", "wlan", "eth"] {
        if lower.contains(kw) { score += 50; }
    }

    let oct = ip.octets();
    match oct[0] {
        192 if oct[1] == 168 => score += 30,
        10 => score += 20,
        172 if (16..=31).contains(&oct[1]) => score += 5,
        _ => {}
    }

    score
}

#[tauri::command]
pub fn get_server_info() -> Result<ServerInfo, String> {
    use local_ip_address::list_afinet_netifas;
    use std::net::IpAddr;

    let netifas = list_afinet_netifas().map_err(|e| e.to_string())?;

    let mut interfaces: Vec<NetInterface> = netifas
        .into_iter()
        .filter_map(|(name, ip)| {
            let v4 = match ip { IpAddr::V4(v) => v, _ => return None };
            if v4.is_loopback() || v4.is_link_local() { return None; }
            Some(NetInterface {
                score: score_interface(&name, &v4),
                name,
                ip: v4.to_string(),
            })
        })
        .collect();

    interfaces.sort_by(|a, b| b.score.cmp(&a.score));

    Ok(ServerInfo { interfaces, port: HTTP_PORT, webrtc_port: MEDIAMTX_WEBRTC_PORT })
}
