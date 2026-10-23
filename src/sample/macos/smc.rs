//! System Management Controller reads: total system power and fan speed.
//!
//! The SMC user client answers 80-byte `SMCKeyData` requests through
//! `IOConnectCallStructMethod`. A read takes two calls (key info, then bytes);
//! the key info never changes, so it is cached per key.

use super::ffi;
use super::Fan;
use std::collections::HashMap;
use std::ffi::c_void;

const KERNEL_INDEX_SMC: u32 = 2;
const CMD_READ_BYTES: u8 = 5;
const CMD_READ_KEYINFO: u8 = 9;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Version {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PowerLimit {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyInfo {
    size: u32,
    kind: u32,
    attributes: u8,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyData {
    key: u32,
    version: Version,
    limit: PowerLimit,
    info: KeyInfo,
    result: u8,
    status: u8,
    command: u8,
    data32: u32,
    bytes: [u8; 32],
}

const _: () = assert!(std::mem::size_of::<KeyData>() == 80);

pub struct Smc {
    connection: ffi::io_connect_t,
    info: HashMap<[u8; 4], Option<KeyInfo>>,
    fans: Option<u8>,
}

impl Smc {
    pub fn open() -> Option<Self> {
        let matching = unsafe { ffi::IOServiceMatching(c"AppleSMC".as_ptr()) };
        if matching.is_null() {
            return None;
        }
        let service = unsafe { ffi::IOServiceGetMatchingService(ffi::MAIN_PORT_DEFAULT, matching) };
        if service == 0 {
            return None;
        }
        let mut connection = 0;
        let status = unsafe { ffi::IOServiceOpen(service, ffi::task_self(), 0, &mut connection) };
        unsafe { ffi::IOObjectRelease(service) };
        (status == ffi::KERN_SUCCESS).then(|| Self {
            connection,
            info: HashMap::new(),
            fans: None,
        })
    }

    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut output = KeyData::default();
        let mut size = std::mem::size_of::<KeyData>();
        let status = unsafe {
            ffi::IOConnectCallStructMethod(
                self.connection,
                KERNEL_INDEX_SMC,
                input as *const KeyData as *const c_void,
                std::mem::size_of::<KeyData>(),
                &mut output as *mut KeyData as *mut c_void,
                &mut size,
            )
        };
        (status == ffi::KERN_SUCCESS && output.result == 0).then_some(output)
    }

    fn key_info(&mut self, key: [u8; 4]) -> Option<KeyInfo> {
        if let Some(info) = self.info.get(&key) {
            return *info;
        }
        let input = KeyData {
            key: u32::from_be_bytes(key),
            command: CMD_READ_KEYINFO,
            ..Default::default()
        };
        let info = self
            .call(&input)
            .map(|output| output.info)
            .filter(|info| info.size > 0);
        self.info.insert(key, info);
        info
    }

    fn read_raw(&mut self, key: [u8; 4]) -> Option<(KeyInfo, [u8; 32])> {
        let info = self.key_info(key)?;
        let input = KeyData {
            key: u32::from_be_bytes(key),
            info,
            command: CMD_READ_BYTES,
            ..Default::default()
        };
        self.call(&input).map(|output| (info, output.bytes))
    }

    pub fn read_f32(&mut self, key: [u8; 4]) -> Option<f32> {
        let (info, bytes) = self.read_raw(key)?;
        decode(info.kind.to_be_bytes(), info.size as usize, &bytes)
    }

    pub fn fan(&mut self) -> Option<Fan> {
        let count = match self.fans {
            Some(count) => count,
            None => {
                let count = self.read_f32(*b"FNum").map_or(0, |n| n as u8);
                self.fans = Some(count);
                count
            }
        };
        // Report the busiest fan; on laptops both track each other closely.
        let mut best: Option<Fan> = None;
        for index in 0..count.min(4) {
            let digit = b'0' + index;
            let Some(rpm) = self.read_f32([b'F', digit, b'A', b'c']) else {
                continue;
            };
            let fan = Fan { rpm: rpm.max(0.0) };
            if best.is_none_or(|current| fan.rpm > current.rpm) {
                best = Some(fan);
            }
        }
        best
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe { ffi::IOServiceClose(self.connection) };
    }
}

fn decode(kind: [u8; 4], size: usize, bytes: &[u8; 32]) -> Option<f32> {
    match &kind {
        // Apple Silicon publishes native little-endian floats.
        b"flt " if size == 4 => Some(f32::from_le_bytes(bytes[..4].try_into().ok()?)),
        b"fpe2" if size == 2 => Some(u16::from_be_bytes([bytes[0], bytes[1]]) as f32 / 4.0),
        b"sp78" if size == 2 => Some(i16::from_be_bytes([bytes[0], bytes[1]]) as f32 / 256.0),
        b"ui8 " => Some(bytes[0] as f32),
        b"ui16" => Some(u16::from_be_bytes([bytes[0], bytes[1]]) as f32),
        b"ui32" => Some(u32::from_be_bytes(bytes[..4].try_into().ok()?) as f32),
        _ => None,
    }
}
