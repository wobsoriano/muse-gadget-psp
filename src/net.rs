//! Wi-Fi, name lookup and TCP on the PSP.
//!
//! Sockets stay non-blocking and every wait goes through `sceNetInetPoll`
//! with a limit, and names are looked up here over UDP. On a real PSP the
//! system resolver sometimes never returned, and a blocking socket has no
//! limit to give up at.

use crate::entropy;
use alloc::{format, string::String, vec::Vec};
use core::ffi::c_void;
use muse_link::{dns, Lost, Stream, Wire};
use psp::sys::{self, sockaddr, ApctlInfo, NetModule, SceNetApctlInfo, SceNetInetPollfd};

const AF_INET: u8 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOL_SOCKET: i32 = 0xffff;
const SO_NONBLOCK: i32 = 0x1009;
const SO_ERROR: i32 = 0x1007;
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;

const WIFI_DISCONNECTED: i32 = 0;
const WIFI_GOT_ADDRESS: i32 = 4;

const PATIENCE_MS: i32 = 15_000;
/// A send that takes nothing for this long has not been seen to recover,
/// and the question it carried can be asked again sooner.
const STALLED_MS: i32 = 8_000;
const LOOKUP_WAIT_MS: i32 = 1_500;
/// The most handed to the socket in one call, each sent at once. A real PSP
/// stalled for whole seconds on large uploads until they went out this way.
const SEND_AT_MOST: usize = 512;
const FALLBACK_NAME_SERVERS: [[u8; 4]; 2] = [[1, 1, 1, 1], [8, 8, 8, 8]];
const IPPROTO_TCP: i32 = 6;
const TCP_NODELAY: i32 = 1;

fn step(name: &str, result: i32) -> Result<(), String> {
    if result < 0 {
        return Err(format!("{} failed: {:#x}", name, result));
    }
    Ok(())
}

/// Loads and starts the system's network stack.
pub fn start() -> Result<(), String> {
    unsafe {
        step("load common", sys::sceUtilityLoadNetModule(NetModule::NetCommon))?;
        step("load inet", sys::sceUtilityLoadNetModule(NetModule::NetInet))?;
        step("net init", sys::sceNetInit(128 * 1024, 42, 4 * 1024, 42, 4 * 1024))?;
        step("inet init", sys::sceNetInetInit())?;
        step("apctl init", sys::sceNetApctlInit(0x8000, 48))
    }
}

/// Joins the saved connection numbered `profile` in the PSP's Network Settings.
pub fn join(profile: i32) -> Result<(), String> {
    step("wifi connect", unsafe { sys::sceNetApctlConnect(profile) })?;
    let mut last = -1;
    // About 20 seconds to associate and get an address.
    for tick in 0..400 {
        let mut state = 0i32;
        step("wifi state", unsafe { sys::sceNetApctlGetState(&mut state as *mut i32 as *mut sys::ApctlState) })?;
        if state != last {
            entropy::stir(&state.to_le_bytes());
            last = state;
        }
        if state == WIFI_GOT_ADDRESS {
            return Ok(());
        }
        // Dropping back to disconnected after starting means the join failed.
        if state == WIFI_DISCONNECTED && tick > 40 {
            return Err("Wi-Fi did not join. Is the switch on?".into());
        }
        unsafe { sys::sceKernelDelayThread(50_000) };
    }
    Err("Wi-Fi took too long to join".into())
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut parts = text.split('.');
    let mut address = [0u8; 4];
    for byte in &mut address {
        *byte = parts.next()?.parse().ok()?;
    }
    (parts.next().is_none() && address != [0; 4]).then_some(address)
}

fn configured_name_server(which: ApctlInfo) -> Option<[u8; 4]> {
    let mut info: SceNetApctlInfo = unsafe { core::mem::zeroed() };
    if unsafe { sys::sceNetApctlGetInfo(which, &mut info) } != 0 {
        return None;
    }
    // Both name server fields are the same 16 bytes of this union.
    let text = unsafe { info.primary_dns };
    let end = text.iter().position(|&byte| byte == 0)?;
    parse_ipv4(core::str::from_utf8(&text[..end]).ok()?)
}

fn peer(address: [u8; 4], port: u16) -> sockaddr {
    let mut peer = sockaddr { sa_len: 16, sa_family: AF_INET, sa_data: [0; 14] };
    peer.sa_data[..2].copy_from_slice(&port.to_be_bytes());
    peer.sa_data[2..6].copy_from_slice(&address);
    peer
}

fn ready(fd: i32, events: i16, wait_ms: i32) -> bool {
    let mut poll = SceNetInetPollfd { fd, events, revents: 0 };
    unsafe { sys::sceNetInetPoll(&mut poll, 1, wait_ms) > 0 }
}

pub struct Socket(i32);

impl Socket {
    fn new(kind: i32) -> Result<Socket, String> {
        let fd = unsafe { sys::sceNetInetSocket(AF_INET as i32, kind, 0) };
        step("socket", fd)?;
        let socket = Socket(fd);
        let on = 1i32;
        step("non-blocking", unsafe {
            sys::sceNetInetSetsockopt(fd, SOL_SOCKET, SO_NONBLOCK, &on as *const i32 as *const c_void, 4)
        })?;
        Ok(socket)
    }

    pub fn connect(address: [u8; 4], port: u16) -> Result<Socket, String> {
        let socket = Socket::new(SOCK_STREAM)?;
        let on = 1i32;
        let ungathered =
            unsafe { sys::sceNetInetSetsockopt(socket.0, IPPROTO_TCP, TCP_NODELAY, &on as *const i32 as *const c_void, 4) };
        if ungathered < 0 {
            crate::say!("net: could not turn off send gathering: {:#x}", ungathered);
        }
        let peer = peer(address, port);
        if unsafe { sys::sceNetInetConnect(socket.0, &peer, 16) } < 0 {
            let errno = unsafe { sys::sceNetInetGetErrno() };
            if !ready(socket.0, POLLOUT, PATIENCE_MS) {
                return Err(format!("the server did not answer (errno {})", errno));
            }
            let (mut failure, mut size) = (0i32, 4u32);
            let read = unsafe {
                sys::sceNetInetGetsockopt(socket.0, SOL_SOCKET, SO_ERROR, &mut failure as *mut i32 as *mut c_void, &mut size)
            };
            if read < 0 || failure != 0 {
                return Err(format!("the server refused the connection ({})", failure));
            }
        }
        Ok(socket)
    }
}

/// The socket has no room, or nothing to give, at this instant. Not a failure.
fn would_block() -> Option<i32> {
    const EINTR: i32 = 4;
    const EAGAIN: i32 = 11;
    let errno = unsafe { sys::sceNetInetGetErrno() };
    (errno != EAGAIN && errno != EINTR).then_some(errno)
}

impl Stream for Socket {
    fn read(&mut self, into: &mut [u8]) -> Result<usize, Lost> {
        let started = unsafe { sys::sceKernelGetSystemTimeLow() };
        loop {
            if !ready(self.0, POLLIN, PATIENCE_MS) {
                crate::say!("net: nothing to read for {} s", PATIENCE_MS / 1000);
                return Err(Lost);
            }
            let got = unsafe { sys::sceNetInetRecv(self.0, into.as_mut_ptr() as *mut c_void, into.len(), 0) };
            entropy::stir(&got.to_le_bytes());
            if let Some(count) = usize::try_from(got).ok().filter(|&count| count <= into.len()) {
                return Ok(count);
            }
            let waited_ms = unsafe { sys::sceKernelGetSystemTimeLow() }.wrapping_sub(started) / 1000;
            if let Some(errno) = would_block().or((waited_ms > PATIENCE_MS as u32).then_some(0)) {
                crate::say!("net: read failed, result {} errno {}", got, errno);
                return Err(Lost);
            }
        }
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), Lost> {
        // A PSP socket can report room and then take nothing. That is a wait,
        // and only silence past the limit is a loss.
        let mut progressed = unsafe { sys::sceKernelGetSystemTimeLow() };
        while !bytes.is_empty() {
            if !ready(self.0, POLLOUT, STALLED_MS) {
                let mut poll = SceNetInetPollfd { fd: self.0, events: POLLOUT | POLLIN, revents: 0 };
                let result = unsafe { sys::sceNetInetPoll(&mut poll, 1, 0) };
                crate::say!("net: no room to write for {} s, {} bytes left, poll {} events {:#x}", STALLED_MS / 1000, bytes.len(), result, poll.revents);
                return Err(Lost);
            }
            let piece = bytes.len().min(SEND_AT_MOST);
            let sent = unsafe { sys::sceNetInetSend(self.0, bytes.as_ptr() as *const c_void, piece, 0) };
            let now = unsafe { sys::sceKernelGetSystemTimeLow() };
            match usize::try_from(sent) {
                Ok(count) if count > 0 && count <= piece => {
                    bytes = &bytes[count..];
                    progressed = now;
                }
                _ => {
                    let stalled_ms = now.wrapping_sub(progressed) / 1000;
                    let failure = if sent == 0 { None } else { would_block() };
                    if let Some(errno) = failure.or((stalled_ms > STALLED_MS as u32).then_some(0)) {
                        crate::say!("net: write failed, result {} errno {}, {} bytes left", sent, errno, bytes.len());
                        return Err(Lost);
                    }
                    unsafe { sys::sceKernelDelayThread(5_000) };
                }
            }
        }
        Ok(())
    }
}

impl Wire for Socket {
    fn readable(&mut self, wait_ms: u32) -> Result<bool, Lost> {
        let mut poll = SceNetInetPollfd { fd: self.0, events: POLLIN, revents: 0 };
        match unsafe { sys::sceNetInetPoll(&mut poll, 1, wait_ms as i32) } {
            waiting if waiting < 0 => Err(Lost),
            // A hang-up counts as readable, so the read that follows sees it.
            waiting => Ok(waiting > 0),
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe { sys::sceNetInetClose(self.0) };
    }
}

fn ask(socket: &Socket, server: [u8; 4], host: &str) -> Option<[u8; 4]> {
    let mut id = [0u8; 2];
    getrandom::getrandom(&mut id).ok()?;
    let id = u16::from_le_bytes(id);
    let mut packet = [0u8; 512];
    let length = dns::query(&mut packet, host, id)?;
    let to = peer(server, 53);
    let sent = unsafe { sys::sceNetInetSendto(socket.0, packet.as_ptr() as *const c_void, length, 0, &to, 16) };
    if sent != length as isize || !ready(socket.0, POLLIN, LOOKUP_WAIT_MS) {
        return None;
    }
    let (mut from, mut from_size) = (to, 16u32);
    let got = unsafe {
        sys::sceNetInetRecvfrom(socket.0, packet.as_mut_ptr() as *mut c_void, packet.len(), 0, &mut from, &mut from_size)
    };
    let reply = packet.get(..usize::try_from(got).ok()?)?;
    entropy::stir(reply);
    dns::answer(reply, id)
}

/// The network's name and its signal strength out of 100.
pub fn wifi() -> Option<(String, u8)> {
    let mut info: SceNetApctlInfo = unsafe { core::mem::zeroed() };
    if unsafe { sys::sceNetApctlGetInfo(ApctlInfo::Ssid, &mut info) } != 0 {
        return None;
    }
    let name = unsafe { info.ssid };
    let name = String::from_utf8_lossy(&name[..name.iter().position(|&byte| byte == 0).unwrap_or(name.len())]).into_owned();
    if unsafe { sys::sceNetApctlGetInfo(ApctlInfo::Strength, &mut info) } != 0 {
        return None;
    }
    Some((name, unsafe { info.strength }))
}

/// The IPv4 address of `host`, asked of the network's name servers and then
/// of two public ones.
pub fn lookup(host: &str) -> Result<[u8; 4], String> {
    let servers: Vec<[u8; 4]> = [ApctlInfo::PrimaryDns, ApctlInfo::SecondaryDns]
        .into_iter()
        .filter_map(configured_name_server)
        .chain(FALLBACK_NAME_SERVERS)
        .collect();
    let socket = Socket::new(SOCK_DGRAM)?;
    for _round in 0..2 {
        for &server in &servers {
            if let Some(address) = ask(&socket, server, host) {
                return Ok(address);
            }
        }
    }
    Err(format!("no name server answered for {}", host))
}
