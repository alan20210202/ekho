#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::type_complexity)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use crate::config::get_config;
use bytes::{Bytes, BytesMut};
use crossbeam_channel::Sender;
use dashmap::DashMap;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use priority_queue::PriorityQueue;
use std::cmp::Reverse;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind, Result};
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::ptr::slice_from_raw_parts;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

//==================================================================================================
//                                Wrapper around the unsafe C FFI
//==================================================================================================

unsafe extern "C" fn output_callback(
    buf: *const c_char,
    len: c_int,
    kcp: *mut ikcpcb,
    user: *mut c_void,
) -> c_int {
    let obj = user as *mut KcpControlBlock;
    assert_ne!(obj, std::ptr::null_mut());
    assert_eq!(kcp, (*obj).inner);
    let bytes = Bytes::copy_from_slice(&*slice_from_raw_parts(buf as *const u8, len as usize));
    (*obj).sender.send(bytes).unwrap();
    len
}

pub fn get_conv(block: &[u8]) -> u32 {
    unsafe { ikcp_getconv(block.as_ptr() as *const c_void) }
}

/// A thin wrapper above KCP
#[derive(Debug)]
pub struct KcpControlBlock {
    inner: *mut ikcpcb,
    sender: Sender<Bytes>,
}

unsafe impl Send for KcpControlBlock {}

unsafe impl Sync for KcpControlBlock {}

impl KcpControlBlock {
    pub fn new_with_sender(conv: u32, sender: Sender<Bytes>) -> Box<KcpControlBlock> {
        let mut ret = Box::new(KcpControlBlock {
            inner: std::ptr::null_mut(),
            sender,
        });
        ret.inner = unsafe {
            ikcp_create(
                conv as IUINT32,
                &mut *ret as *mut KcpControlBlock as *mut c_void,
            )
        };
        unsafe { ikcp_setoutput(ret.inner, Some(output_callback)) };
        ret
    }

    pub fn new(conv: u32) -> Box<KcpControlBlock> {
        Self::new_with_sender(conv, crate::icmp::get_sender())
    }

    pub fn conv(&self) -> u32 {
        unsafe { (*self.inner).conv }
    }

    pub fn check(&self, time: u32) -> u32 {
        unsafe { ikcp_check(self.inner, time as IUINT32) }
    }

    pub fn update(&mut self, time: u32) {
        unsafe {
            ikcp_update(self.inner, time as IUINT32);
        }
    }

    pub fn input(&mut self, data: &[u8]) {
        unsafe {
            ikcp_input(
                self.inner,
                data.as_ptr() as *const c_char,
                data.len() as c_long,
            );
        }
    }

    pub fn peek_size(&self) -> i32 {
        unsafe { ikcp_peeksize(self.inner) as i32 }
    }

    pub fn set_mtu(&mut self, mtu: usize) {
        unsafe {
            ikcp_setmtu(self.inner, mtu as c_int);
        }
    }

    pub fn set_window_size(&mut self, send: u32, recv: u32) {
        unsafe {
            ikcp_wndsize(self.inner, send as c_int, recv as c_int);
        }
    }

    pub fn set_nodelay(&mut self, nodelay: bool, interval: u32, resend: u32, nc: bool) {
        let _ = unsafe {
            ikcp_nodelay(
                self.inner,
                nodelay as c_int,
                interval as c_int,
                resend as c_int,
                nc as c_int,
            )
        };
    }

    pub fn send(&mut self, data: &[u8]) -> i32 {
        unsafe {
            ikcp_send(
                self.inner,
                data.as_ptr() as *const c_char,
                data.len() as c_int,
            ) as i32
        }
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> i32 {
        unsafe {
            ikcp_recv(
                self.inner,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as c_int,
            ) as i32
        }
    }
}

impl Drop for KcpControlBlock {
    fn drop(&mut self) {
        unsafe { ikcp_release(self.inner) };
    }
}

//==================================================================================================
//                                     KCP Update Scheduling
//==================================================================================================

#[derive(Debug, Clone)]
struct KcpSchedulerItem(Arc<Mutex<Box<KcpControlBlock>>>);

impl Eq for KcpSchedulerItem {}

impl PartialEq for KcpSchedulerItem {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Hash for KcpSchedulerItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

lazy_static! {
    static ref UPDATE_SCHEDULE: Mutex<PriorityQueue<KcpSchedulerItem, Reverse<u32>>> =
        Mutex::new(PriorityQueue::new());
}

pub fn schedule_immediate_update(target: Arc<Mutex<Box<KcpControlBlock>>>) {
    let mut guard = UPDATE_SCHEDULE.lock();
    guard.push_increase(KcpSchedulerItem(target), Reverse(0));
}

pub fn init_kcp_scheduler() {
    let interval = get_config().kcp.scheduler_interval;
    thread::spawn(|| loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;
        {
            let mut guard = UPDATE_SCHEDULE.lock();
            while guard
                .peek()
                .map(|item| *item.1 >= Reverse(now))
                .unwrap_or(false)
            {
                let (update, _) = guard.pop().unwrap();
                let mut kcp = update.0.lock();
                kcp.update(now);
                guard.push(KcpSchedulerItem(update.0.clone()), Reverse(kcp.check(now)));
            }
        }
        thread::sleep(Duration::from_millis(interval as u64));
    });
}

//==================================================================================================
//                                    Connection Management
//==================================================================================================

lazy_static! {
    static ref CONNECTION_STATE: DashMap<u32, Arc<Mutex<Box<KcpControlBlock>>>> = DashMap::new();
}

struct KcpConnection {
    control: Arc<Mutex<Box<KcpControlBlock>>>,
}

impl KcpConnection {
    pub fn new(conv: u32) -> Result<KcpConnection> {
        if CONNECTION_STATE.contains_key(&conv) {
            return Err(Error::from(ErrorKind::AddrInUse));
        }
        let control = Arc::new(Mutex::new(KcpControlBlock::new(conv)));
        let config = &get_config().kcp;
        control.lock().set_nodelay(
            config.nodelay,
            config.interval,
            config.resend,
            !config.flow_control,
        );
        CONNECTION_STATE.insert(conv, control.clone());
        Ok(KcpConnection { control })
    }

    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        let ret = self.control.lock().send(data);
        if ret < 0 {
            return Err(Error::new(
                ErrorKind::Other,
                format!("KCP internal error {}", ret),
            ));
        }
        schedule_immediate_update(self.control.clone());
        Ok(())
    }

    pub fn try_recv(&mut self) -> Option<Bytes> {
        let mut control = self.control.lock();
        let size = control.peek_size();
        if size < 0 {
            None
        } else {
            let mut ret = BytesMut::with_capacity(size as usize);
            control.recv(ret.as_mut());
            Some(Bytes::from(ret))
        }
    }
}

impl Drop for KcpConnection {
    fn drop(&mut self) {
        CONNECTION_STATE.remove(&self.control.lock().conv());
    }
}

pub fn handle_kcp_packet(packet: &[u8]) {
    let conv = get_conv(packet);
    if let Some(control) = CONNECTION_STATE.get(&conv) {
        control.lock().input(packet);
        schedule_immediate_update(control.clone());
    }
}