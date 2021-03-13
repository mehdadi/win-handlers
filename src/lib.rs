#![cfg(windows)]
#![allow(dead_code)]

use winapi::shared::ntdef::HANDLE;
use winapi::um::fileapi::*;
use winapi::um::handleapi::*;
use winapi::um::minwinbase::OVERLAPPED;
use winapi::um::namedpipeapi::*;
use winapi::um::synchapi::*;
use winapi::um::winbase::*;
use winapi::um::winnt::*;

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::Arc;

struct Handle {
    value: HANDLE,
}

unsafe impl Sync for Handle {}
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.value) };
    }
}

struct Event {
    handle: Handle,
}

impl Event {
    fn new(manual: bool) -> io::Result<Event> {
        let man = if manual { 0 } else { 1 };

        let handle = unsafe { CreateEventW(ptr::null_mut(), man, 0, ptr::null_mut()) };

        if handle != ptr::null_mut() {
            Ok(Event {
                handle: Handle { value: handle },
            })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn reset(&self) -> io::Result<()> {
        let res = unsafe { ResetEvent(self.handle.value) };
        if res != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn set(&self) -> io::Result<()> {
        let res = unsafe { SetEvent(self.handle.value) };
        if res != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    fn wait_forever(&self) -> io::Result<bool> {
        let result = unsafe { WaitForSingleObject(self.handle.value, INFINITE) };
        match result {
            winapi::um::winbase::WAIT_OBJECT_0 => Ok(true),
            winapi::shared::winerror::WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

struct Overlapped {
    ovl: Box<OVERLAPPED>,
    event: Event,
}

impl Overlapped {
    fn new() -> io::Result<Overlapped> {
        let event = Event::new(true);
        //TODO: Check if reseting cause mem leak of this new boxing
        let mut ovl: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        match event {
            Ok(e) => {
                ovl.hEvent = e.handle.value;
                Ok(Overlapped { ovl: ovl, event: e })
            }
            Err(e) => Err(e),
        }
    }

    fn get(&mut self) -> &mut OVERLAPPED {
        &mut self.ovl
    }
}

unsafe impl Send for Overlapped {}
unsafe impl Sync for Overlapped {}

//---------------STARTIGN NAME PIPE ----------------------------------//

const IO_SIZE: u32 = 65536;

struct NamedPipeFactory {
    name: Arc<Vec<u16>>,
}

impl NamedPipeFactory {
    fn new<T: AsRef<OsStr>>(name: T) -> NamedPipeFactory {
        let mut fullname: OsString = name.as_ref().into();
        fullname.push("\x00");
        let fullname = fullname.encode_wide().collect::<Vec<u16>>();
        NamedPipeFactory {
            name: Arc::new(fullname),
        }
    }

    fn connect(&self, handle: &Handle, ovl: &mut Overlapped) -> io::Result<bool> {
        let res = unsafe { ConnectNamedPipe(handle.value, ovl.get()) };
        let err = io::Error::last_os_error();
        if res == winapi::shared::minwindef::TRUE {
            //OVERLAPPED returns FALSE
            Err(err)
        } else {
            match err.raw_os_error().unwrap() as u32 {
                winapi::shared::winerror::ERROR_IO_PENDING => Ok(false),
                winapi::shared::winerror::ERROR_PIPE_CONNECTED => {
                    ovl.event.set()?;
                    Ok(true)
                }
                _ => Err(err),
            }
        }
    }

    /* TODO: Missing security Attribs, Should be passed to server
     * We may need a factory for the "winapi::um::minwinbase::LPSECURITY_ATTRIBUTES"
     */
    pub fn create_pipe_server(&self) -> io::Result<NamedPipe> {
        let handle = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                IO_SIZE,
                IO_SIZE,
                0,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            let mut ovl = Overlapped::new()?;
            let hnd = Handle { value: handle };
            let connected = self.connect(&hnd, &mut ovl)?;
            Ok(NamedPipe {
                handle: hnd,
                is_connected: connected,
                ovl: ovl,
            })
        }
    }

    fn create_pipe_client(&self) -> io::Result<NamedPipe> {
        loop {
            // Reseting manuall the Error to get the relevant one
            unsafe { winapi::um::errhandlingapi::SetLastError(0) };
            let handle = unsafe {
                CreateFileW(
                    self.name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null_mut(),
                    winapi::um::fileapi::OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };

            if handle != INVALID_HANDLE_VALUE {
                //NAME PIPE READY
                return Ok(NamedPipe {
                    handle: Handle { value: handle },
                    ovl: Overlapped::new()?,
                    is_connected: true,
                });
            }

            let err = io::Error::last_os_error();
            match err.raw_os_error().unwrap() as u32 {
                winapi::shared::winerror::ERROR_PIPE_BUSY => {
                    let res = unsafe { WaitNamedPipeW(self.name.as_ptr(), INFINITE) };
                    if res == winapi::shared::minwindef::FALSE {
                        return Err(io::Error::last_os_error());
                    }
                }
                _ => {
                    return Err(err);
                }
            }
        }
    }
}

struct NamedPipe {
    handle: Handle,
    ovl: Overlapped,
    is_connected: bool,
}

impl NamedPipe {
    fn init_write<'b>(&self, buf: &'b [u8]) -> io::Result<()> {
        assert!(buf.len() <= 0xFFFFFFFF);
        assert!(self.is_connected);

        let mut bytes_written = 0;
        let res = unsafe {
            WriteFile(
                self.handle.value,
                buf.as_ptr() as *mut winapi::ctypes::c_void,
                buf.len() as u32,
                &mut bytes_written,
                ptr::null_mut() //missing overlapepd
            )
        };

        if res == winapi::shared::minwindef::TRUE && 
        bytes_written == buf.len() as u32 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl io::Read for NamedPipe {
    fn read(&mut self, _: &mut [u8]) -> std::result::Result<usize, std::io::Error> {
        todo!()
    }
}

impl io::Write for NamedPipe {
    fn write(&mut self, _: &[u8]) -> std::result::Result<usize, std::io::Error> {
        let write_handle = unsafe {self. }
    }
    fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
        todo!()
    }
}

impl Drop for NamedPipe {
    fn drop(&mut self) {
        //TODO: needs more check on drop
        if self.is_connected {
            let _ = unsafe { DisconnectNamedPipe(self.handle.value) };
        }
    }
}

#[no_mangle]
pub extern "C" fn create_pipe_server(){

}

#[test]
fn event_handles() {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    let ev = Event::new(true).unwrap();
    let a = Arc::new(ev);

    let a1 = a.clone();
    let t1 = ::std::thread::spawn(move || {
        println!("waiting for event in thread");
        a1.wait_forever().unwrap();
        println!("event has exited wait set");
    });

    let a2 = a.clone();
    let t2 = ::std::thread::spawn(move || {
        let mut l = 3;
        loop {
            println!("Trigger set in {} second", l);
            thread::sleep(Duration::from_millis(1000));
            l = l - 1;
            if l == 0 {
                break;
            }
        }
        a2.set().unwrap();
    });

    t1.join().unwrap();
    t2.join().unwrap();
}

#[test]
fn pipe_creation_shared_factory() {
    use std::time::Duration;

    let factory = NamedPipeFactory::new(r"\\.\\pipe\\test_pipe_creation");
    let a = Arc::new(factory);

    let a1 = a.clone();
    let t1 = ::std::thread::spawn(move || {
        let server = a1.create_pipe_server().unwrap();
        println!("Waiting for client to connect");
        server.ovl.event.wait_forever().unwrap();
        println!("Server connected");
    });

    let a2 = a.clone();
    let t2 = ::std::thread::spawn(move || {
        let mut l = 3;
        loop {
            println!("client will connect in {} second", l);
            std::thread::sleep(Duration::from_millis(1000));
            l = l - 1;
            if l == 0 {
                break;
            }
        }
        let _client = a2.create_pipe_client().unwrap();
        println!("client has been connected");
    });

    t1.join().unwrap();
    t2.join().unwrap();
}
