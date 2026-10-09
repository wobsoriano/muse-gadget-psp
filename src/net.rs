//! Wi-Fi, name lookup and TCP on the PSP.
//!
//! Sockets never block. A call that finds nothing to do says so at once,
//! and the wait is a short sleep and another try, up to a limit. The system
//! has a call that waits on a socket with a limit, but the PSP library does
//! not declare it, and a blocking socket has no limit to give up at. Names
//! are looked up here over UDP, because on a real PSP the system resolver
//! sometimes never returned.

use crate::entropy;
use alloc::{format, string::String, vec::Vec};
use core::ffi::c_void;
use muse_link::{dns, Lost, Stream, Wire};
use psp::sys::{self, sockaddr, ApctlInfo, NetModule, SceNetApctlInfo};

const AF_INET: u8 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOL_SOCKET: i32 = 0xffff;
const SO_NONBLOCK: i32 = 0x1009;
const SO_ERROR: i32 = 0x1007;

const WIFI_DISCONNECTED: i32 = 0;
const WIFI_GOT_ADDRESS: i32 = 4;

const PATIENCE_MS: u32 = 15_000;
/// A send that takes nothing for this long has not been seen to recover,
/// and the question it carried can be asked again sooner.
const STALLED_MS: u32 = 8_000;
const LOOKUP_WAIT_MS: u32 = 1_500;
/// How long a socket with nothing to do is left alone before the next try.
const TRY_EVERY_MICROS: u32 = 5_000;
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

/// What one try at moving bytes through a socket came to.
enum Tried {
    Moved(usize),
    /// Nothing to read, or no room to write, at this instant.
    Nothing,
    /// The other side has closed. Only a read can find this out.
    Closed,
    Failed(i32),
}

impl Tried {
    fn of(result: isize, most: usize, reading: bool) -> Tried {
        // The PSP's own numbers for "try again" and "interrupted", and the
        // BSD number for the first in case a firmware reports that one.
        const NOT_NOW: [i32; 3] = [11, 4, 35];
        match usize::try_from(result) {
            Ok(0) if reading => Tried::Closed,
            Ok(0) => Tried::Nothing,
            Ok(count) if count <= most => Tried::Moved(count),
            _ => match unsafe { sys::sceNetInetGetErrno() } {
                errno if NOT_NOW.contains(&errno) => Tried::Nothing,
                errno => Tried::Failed(errno),
            },
        }
    }
}

fn micros() -> u32 {
    unsafe { sys::sceKernelGetSystemTimeLow() }
}

/// Runs `attempt` until it has an answer, sleeping between tries, and gives
/// up with `None` after `limit_ms`.
fn until<T>(limit_ms: u32, mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let started = micros();
    loop {
        if let Some(answer) = attempt() {
            return Some(answer);
        }
        if micros().wrapping_sub(started) / 1000 >= limit_ms {
            return None;
        }
        unsafe { sys::sceKernelDelayThread(TRY_EVERY_MICROS) };
    }
}

pub struct Socket {
    fd: i32,
    /// A byte `readable` had to take to learn there was one.
    taken: Option<u8>,
    closed: bool,
}

impl Socket {
    fn new(kind: i32) -> Result<Socket, String> {
        let fd = unsafe { sys::sceNetInetSocket(AF_INET as i32, kind, 0) };
        step("socket", fd)?;
        let socket = Socket { fd, taken: None, closed: false };
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
            unsafe { sys::sceNetInetSetsockopt(socket.fd, IPPROTO_TCP, TCP_NODELAY, &on as *const i32 as *const c_void, 4) };
        if ungathered < 0 {
            crate::say!("net: could not turn off send gathering: {:#x}", ungathered);
        }
        let peer = peer(address, port);
        if unsafe { sys::sceNetInetConnect(socket.fd, &peer, 16) } >= 0 {
            return Ok(socket);
        }
        // The connection is being made. It is there once the socket can
        // name who it is connected to, and it has failed once the socket
        // holds an error.
        let made = until(PATIENCE_MS, || {
            let (mut failure, mut size) = (0i32, 4u32);
            let read = unsafe {
                sys::sceNetInetGetsockopt(socket.fd, SOL_SOCKET, SO_ERROR, &mut failure as *mut i32 as *mut c_void, &mut size)
            };
            if read < 0 || failure != 0 {
                return Some(Err(format!("the server refused the connection ({})", failure)));
            }
            let (mut who, mut who_size) = (peer, 16u32);
            (unsafe { sys::sceNetInetGetpeername(socket.fd, &mut who, &mut who_size) } >= 0).then_some(Ok(()))
        });
        made.unwrap_or_else(|| Err("the server did not answer".into())).map(|()| socket)
    }

    fn receive(&mut self, into: &mut [u8]) -> Tried {
        let got = unsafe { sys::sceNetInetRecv(self.fd, into.as_mut_ptr() as *mut c_void, into.len(), 0) };
        let tried = Tried::of(got, into.len(), true);
        match tried {
            Tried::Moved(_) => entropy::stir(&micros().to_le_bytes()),
            Tried::Closed => self.closed = true,
            Tried::Nothing | Tried::Failed(_) => {}
        }
        tried
    }
}

impl Stream for Socket {
    fn read(&mut self, into: &mut [u8]) -> Result<usize, Lost> {
        if into.is_empty() {
            return Ok(0);
        }
        if let Some(byte) = self.taken.take() {
            into[0] = byte;
            return Ok(match self.receive(&mut into[1..]) {
                Tried::Moved(more) => 1 + more,
                _ => 1,
            });
        }
        if self.closed {
            return Ok(0);
        }
        let read = until(PATIENCE_MS, || match self.receive(into) {
            Tried::Moved(count) => Some(Ok(count)),
            Tried::Closed => Some(Ok(0)),
            Tried::Nothing => None,
            Tried::Failed(errno) => {
                crate::say!("net: read failed, errno {}", errno);
                Some(Err(Lost))
            }
        });
        read.unwrap_or_else(|| {
            crate::say!("net: nothing to read for {} s", PATIENCE_MS / 1000);
            Err(Lost)
        })
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), Lost> {
        while !bytes.is_empty() {
            let piece = bytes.len().min(SEND_AT_MOST);
            let sent = until(STALLED_MS, || {
                let sent = unsafe { sys::sceNetInetSend(self.fd, bytes.as_ptr() as *const c_void, piece, 0) };
                match Tried::of(sent, piece, false) {
                    Tried::Moved(count) => Some(Ok(count)),
                    Tried::Nothing | Tried::Closed => None,
                    Tried::Failed(errno) => Some(Err(errno)),
                }
            });
            match sent {
                Some(Ok(count)) => bytes = &bytes[count..],
                Some(Err(errno)) => {
                    crate::say!("net: write failed, errno {}, {} bytes left", errno, bytes.len());
                    return Err(Lost);
                }
                None => {
                    crate::say!("net: no room to write for {} s, {} bytes left", STALLED_MS / 1000, bytes.len());
                    return Err(Lost);
                }
            }
        }
        Ok(())
    }
}

impl Wire for Socket {
    fn readable(&mut self, wait_ms: u32) -> Result<bool, Lost> {
        // A hang-up counts as readable, so the read that follows sees it.
        if self.taken.is_some() || self.closed {
            return Ok(true);
        }
        let mut byte = [0u8; 1];
        let found = until(wait_ms, || match self.receive(&mut byte) {
            Tried::Moved(_) => Some(Ok(Some(byte[0]))),
            Tried::Closed => Some(Ok(None)),
            Tried::Nothing => None,
            Tried::Failed(_) => Some(Err(Lost)),
        });
        match found {
            Some(Ok(taken)) => {
                self.taken = taken;
                Ok(true)
            }
            Some(Err(lost)) => Err(lost),
            None => Ok(false),
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe { sys::sceNetInetClose(self.fd) };
    }
}

fn ask(socket: &Socket, server: [u8; 4], host: &str) -> Option<[u8; 4]> {
    let mut id = [0u8; 2];
    getrandom::getrandom(&mut id).ok()?;
    let id = u16::from_le_bytes(id);
    let mut packet = [0u8; 512];
    let length = dns::query(&mut packet, host, id)?;
    let to = peer(server, 53);
    let sent = unsafe { sys::sceNetInetSendto(socket.fd, packet.as_ptr() as *const c_void, length, 0, &to, 16) };
    if sent != length as isize {
        return None;
    }
    let got = until(LOOKUP_WAIT_MS, || {
        let (mut from, mut from_size) = (to, 16u32);
        let got = unsafe {
            sys::sceNetInetRecvfrom(socket.fd, packet.as_mut_ptr() as *mut c_void, packet.len(), 0, &mut from, &mut from_size)
        };
        match Tried::of(got, packet.len(), true) {
            Tried::Moved(count) => Some(Some(count)),
            Tried::Nothing => None,
            Tried::Closed | Tried::Failed(_) => Some(None),
        }
    })??;
    let reply = &packet[..got];
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
