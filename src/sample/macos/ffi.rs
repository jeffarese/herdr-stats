//! Hand-written bindings for the slice of Mach, IOKit and CoreFoundation the
//! samplers use. Kept local rather than pulling a binding crate per framework.

#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{
    kCFAllocatorDefault, CFAllocatorRef, CFGetTypeID, CFIndex, CFRelease, CFTypeRef,
};
use core_foundation_sys::dictionary::{
    CFDictionaryGetValue, CFDictionaryRef, CFMutableDictionaryRef,
};
use core_foundation_sys::number::{
    kCFBooleanTrue, kCFNumberDoubleType, kCFNumberSInt64Type, CFBooleanGetTypeID,
    CFNumberGetTypeID, CFNumberGetValue, CFNumberRef,
};
use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringCreateWithBytes, CFStringGetCString, CFStringGetTypeID,
    CFStringRef,
};
use std::ffi::{c_char, c_void};

pub type mach_port_t = u32;
pub type kern_return_t = i32;
pub type io_object_t = mach_port_t;
pub type io_iterator_t = io_object_t;
pub type io_connect_t = io_object_t;

pub const KERN_SUCCESS: kern_return_t = 0;
pub const MAIN_PORT_DEFAULT: mach_port_t = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    pub fn IOServiceGetMatchingService(main: mach_port_t, matching: CFDictionaryRef)
        -> io_object_t;
    pub fn IOServiceGetMatchingServices(
        main: mach_port_t,
        matching: CFDictionaryRef,
        existing: *mut io_iterator_t,
    ) -> kern_return_t;
    pub fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
    pub fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    pub fn IORegistryEntryCreateCFProperty(
        entry: io_object_t,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    pub fn IOServiceOpen(
        service: io_object_t,
        owning_task: mach_port_t,
        kind: u32,
        connect: *mut io_connect_t,
    ) -> kern_return_t;
    pub fn IOServiceClose(connect: io_connect_t) -> kern_return_t;
    pub fn IOConnectCallStructMethod(
        connection: mach_port_t,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> kern_return_t;

    // Private but stable since the first Apple Silicon release; every
    // unprivileged temperature tool on macOS goes through these.
    pub fn IOHIDEventSystemClientCreate(allocator: CFAllocatorRef) -> *const c_void;
    pub fn IOHIDEventSystemClientSetMatching(client: *const c_void, matching: CFDictionaryRef);
    pub fn IOHIDEventSystemClientCopyServices(client: *const c_void) -> CFArrayRef;
    pub fn IOHIDServiceClientCopyProperty(service: *const c_void, key: CFStringRef) -> CFTypeRef;
    pub fn IOHIDServiceClientCopyEvent(
        service: *const c_void,
        kind: i64,
        options: i32,
        timestamp: i64,
    ) -> *const c_void;
    pub fn IOHIDEventGetFloatValue(event: *const c_void, field: i32) -> f64;

    pub fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    pub fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    pub fn IOPSGetPowerSourceDescription(blob: CFTypeRef, source: CFTypeRef) -> CFDictionaryRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFURLVolumeTotalCapacityKey: CFStringRef;
    pub static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFStringRef;
    pub fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFAllocatorRef,
        buffer: *const u8,
        len: CFIndex,
        is_directory: u8,
    ) -> CFTypeRef;
    pub fn CFURLCopyResourcePropertyForKey(
        url: CFTypeRef,
        key: CFStringRef,
        value: *mut CFTypeRef,
        error: *mut CFTypeRef,
    ) -> u8;
}

extern "C" {
    pub static mach_task_self_: mach_port_t;
    pub fn mach_host_self() -> mach_port_t;
    pub fn host_processor_info(
        host: mach_port_t,
        flavor: i32,
        processor_count: *mut u32,
        info: *mut *mut i32,
        info_count: *mut u32,
    ) -> kern_return_t;
    pub fn host_statistics64(
        host: mach_port_t,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
    ) -> kern_return_t;
    pub fn vm_deallocate(task: mach_port_t, address: usize, size: usize) -> kern_return_t;
}

/// A CoreFoundation object this code owns and releases on drop.
pub struct Owned(pub CFTypeRef);

impl Owned {
    /// Wraps a `Create`/`Copy` result, treating null as absent.
    pub fn new(object: CFTypeRef) -> Option<Self> {
        // Not `then_some`: that would build (and then drop, so release) the
        // wrapper even for null.
        if object.is_null() {
            None
        } else {
            Some(Self(object))
        }
    }

    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // A failed Create returns null, and CFRelease(NULL) crashes.
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

pub fn cfstr(text: &str) -> Owned {
    let string = unsafe {
        CFStringCreateWithBytes(
            kCFAllocatorDefault,
            text.as_ptr(),
            text.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        )
    };
    Owned(string as CFTypeRef)
}

pub fn to_string(value: CFTypeRef) -> Option<String> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }
    let mut buffer = [0 as c_char; 256];
    let ok = unsafe {
        CFStringGetCString(
            value as CFStringRef,
            buffer.as_mut_ptr(),
            buffer.len() as CFIndex,
            kCFStringEncodingUTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let text = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(text.to_string_lossy().into_owned())
}

pub fn to_i64(value: CFTypeRef) -> Option<i64> {
    if value.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(value) == CFBooleanGetTypeID() {
            return Some((value == kCFBooleanTrue as CFTypeRef) as i64);
        }
        if CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut out: i64 = 0;
        let ok = CFNumberGetValue(
            value as CFNumberRef,
            kCFNumberSInt64Type,
            &mut out as *mut i64 as *mut c_void,
        );
        if ok {
            Some(out)
        } else {
            let mut real: f64 = 0.0;
            CFNumberGetValue(
                value as CFNumberRef,
                kCFNumberDoubleType,
                &mut real as *mut f64 as *mut c_void,
            )
            .then_some(real as i64)
        }
    }
}

pub fn to_bool(value: CFTypeRef) -> Option<bool> {
    to_i64(value).map(|v| v != 0)
}

/// Borrowed lookup: the result belongs to the dictionary.
pub fn dict_get(dict: CFTypeRef, key: &Owned) -> CFTypeRef {
    if dict.is_null() {
        return std::ptr::null();
    }
    unsafe { CFDictionaryGetValue(dict as CFDictionaryRef, key.as_ptr()) }
}

pub fn array_items(array: CFArrayRef) -> impl Iterator<Item = CFTypeRef> {
    let count = if array.is_null() {
        0
    } else {
        unsafe { CFArrayGetCount(array) }
    };
    (0..count).map(move |index| unsafe { CFArrayGetValueAtIndex(array, index) })
}

pub fn task_self() -> mach_port_t {
    unsafe { mach_task_self_ }
}
