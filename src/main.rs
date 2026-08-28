use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::AsRawFd;
use std::slice;
use std::thread::sleep;
use std::time::{Duration, Instant};

// Linux input event constants. See /usr/include/linux/input-event-codes.h
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_FORWARD: u16 = 0x115;
const BTN_BACK: u16 = 0x116;
const BTN_TASK: u16 = 0x117;

// ioctl request numbers for x86_64/aarch64 Linux.
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

#[repr(C)]
#[derive(Copy, Clone)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UInputUserDev {
    name: [u8; 80],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; 64],
    absmin: [i32; 64],
    absfuzz: [i32; 64],
    absflat: [i32; 64],
}

/// A left-button release we have seen but not yet forwarded. We wait out the
/// debounce window: if the switch bounces back to "pressed" within it, the
/// release was contact bounce and the button must stay logically held.
struct PendingRelease {
    /// The release event, forwarded only if the deadline expires.
    release_ev: InputEvent,
    /// When the release may be confirmed (no re-press before this instant).
    deadline: Instant,
    /// Events that arrived while waiting (movement, other buttons, SYNs).
    /// Replayed in order once the release is confirmed or cancelled.
    buffered: Vec<InputEvent>,
}

fn ioctl_int(fd: i32, request: libc::c_ulong, value: i32) -> io::Result<()> {
    let ret = unsafe { libc::ioctl(fd, request, value) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn ioctl_noarg(fd: i32, request: libc::c_ulong) -> io::Result<()> {
    let ret = unsafe { libc::ioctl(fd, request) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn as_bytes<T>(p: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((p as *const T) as *const u8, size_of::<T>()) }
}

fn read_event(dev: &mut File) -> io::Result<InputEvent> {
    let mut ev: InputEvent = unsafe { zeroed() };
    let buf = unsafe { slice::from_raw_parts_mut((&mut ev as *mut InputEvent) as *mut u8, size_of::<InputEvent>()) };
    dev.read_exact(buf)?;
    Ok(ev)
}

fn write_event(uinput: &mut File, ev: &InputEvent) -> io::Result<()> {
    uinput.write_all(as_bytes(ev))
}

/// Wait up to `timeout` for `fd` to become readable. Returns Ok(true) if an
/// event is available, Ok(false) on timeout.
fn wait_readable(fd: i32, timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ret > 0)
}

fn is_left_button(ev: &InputEvent) -> bool {
    ev.type_ == EV_KEY && ev.code == BTN_LEFT
}

/// Forward everything in the buffer except left-button events (those are
/// swallowed/re-emitted explicitly by the caller). Order is preserved.
fn replay_buffered(uinput: &mut File, buffered: &[InputEvent]) -> io::Result<()> {
    for ev in buffered {
        if !is_left_button(ev) {
            write_event(uinput, ev)?;
        }
    }
    Ok(())
}

fn create_virtual_mouse() -> io::Result<File> {
    let mut ui = OpenOptions::new().write(true).open("/dev/uinput")?;
    let fd = ui.as_raw_fd();

    ioctl_int(fd, UI_SET_EVBIT, EV_KEY as i32)?;
    ioctl_int(fd, UI_SET_EVBIT, EV_REL as i32)?;
    ioctl_int(fd, UI_SET_EVBIT, EV_SYN as i32)?;

    for key in [
        BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA, BTN_FORWARD, BTN_BACK, BTN_TASK,
    ] {
        ioctl_int(fd, UI_SET_KEYBIT, key as i32)?;
    }

    for rel in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
        ioctl_int(fd, UI_SET_RELBIT, rel as i32)?;
    }

    let mut uidev: UInputUserDev = unsafe { zeroed() };
    let name = b"debounced virtual mouse";
    uidev.name[..name.len()].copy_from_slice(name);
    uidev.id = InputId {
        bustype: 0x03, // BUS_USB
        vendor: 0xfeed,
        product: 0xdeb0,
        version: 1,
    };

    ui.write_all(as_bytes(&uidev))?;
    ioctl_noarg(fd, UI_DEV_CREATE)?;
    sleep(Duration::from_millis(100));
    Ok(ui)
}

fn usage(program: &str) -> ! {
    eprintln!(
        "Usage: {program} /dev/input/eventX [debounce-ms]\n\n\
         Example:\n  sudo {program} /dev/input/by-id/usb-Your_Mouse-event-mouse 80\n\n\
         Debounces both edges of BTN_LEFT:\n\
         \x20 * a re-press within the window after a press is a phantom click (dropped)\n\
         \x20 * a re-press within the window after a release is switch bounce while\n\
         \x20   holding (the release is dropped, so drags/holds are not interrupted)\n\n\
         Pick the physical mouse event node, not the virtual mouse this program creates."
    );
    std::process::exit(2);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        usage(&args.get(0).map(String::as_str).unwrap_or("faulty-mouse-debounce"));
    }

    let input_path = &args[1];
    let debounce = Duration::from_millis(
        args.get(2)
            .map(|s| s.parse::<u64>().unwrap_or_else(|_| usage(&args[0])))
            .unwrap_or(80),
    );

    let mut input = OpenOptions::new().read(true).open(input_path)?;
    let input_fd = input.as_raw_fd();

    let mut uinput = create_virtual_mouse()?;
    let uinput_fd = uinput.as_raw_fd();

    // Hide the broken physical mouse from the compositor. We forward acceptable events through uinput.
    ioctl_int(input_fd, EVIOCGRAB, 1)?;

    eprintln!(
        "Debouncing BTN_LEFT on {input_path}; threshold={}ms.\n\
         \x20 Phantom clicks are suppressed and spurious releases while holding are absorbed.\n\
         \x20 Press Ctrl+C to stop.",
        debounce.as_millis()
    );

    let cleanup = || {
        let _ = ioctl_int(input_fd, EVIOCGRAB, 0);
        let _ = ioctl_noarg(uinput_fd, UI_DEV_DESTROY);
    };

    let mut last_accepted_left_press: Option<Instant> = None;
    let mut suppress_current_left_click = false;
    let mut pending: Option<PendingRelease> = None;

    loop {
        // --- 1. Read the next event, using a timed poll while a release is being debounced.
        let ev = if let Some(p) = pending.as_ref() {
            let now = Instant::now();
            if now >= p.deadline {
                // Deadline expired with no re-press: the release was genuine.
                let p = pending.take().unwrap();
                write_event(&mut uinput, &p.release_ev)?;
                replay_buffered(&mut uinput, &p.buffered)?;
                eprintln!("left release confirmed after {}ms (button now up)", debounce.as_millis());
                continue;
            }
            let remaining = p.deadline - now;
            match wait_readable(input_fd, remaining) {
                Ok(true) => match read_event(&mut input) {
                    Ok(ev) => ev,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        cleanup();
                        return Err(e);
                    }
                },
                // Poll timed out; loop again so the deadline check above confirms the release.
                Ok(false) => continue,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    cleanup();
                    return Err(e);
                }
            }
        } else {
            match read_event(&mut input) {
                Ok(ev) => ev,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    cleanup();
                    return Err(e);
                }
            }
        };

        // --- 2. While a release is pending, buffer events and watch for a bounce re-press.
        if pending.is_some() {
            let is_bounce_repress = is_left_button(&ev) && ev.value == 1;
            if is_bounce_repress {
                // The switch flickered to "released" and back while physically held.
                // Swallow the pending release AND this re-press: stay held.
                let p = pending.take().unwrap();
                replay_buffered(&mut uinput, &p.buffered)?;
                eprintln!("suppressed spurious release while holding (switch bounce)");
            } else {
                pending.as_mut().unwrap().buffered.push(ev);
            }
            continue;
        }

        // --- 3. Normal processing (no release pending).
        let mut forward = true;

        if is_left_button(&ev) {
            match ev.value {
                1 => { // press
                    let now = Instant::now();
                    if last_accepted_left_press
                        .map(|t| now.duration_since(t) < debounce)
                        .unwrap_or(false)
                    {
                        // Phantom click: press arrived too soon after the last accepted press.
                        suppress_current_left_click = true;
                        forward = false;
                        eprintln!("suppressed phantom left-click (bounce re-press)");
                    } else {
                        suppress_current_left_click = false;
                        last_accepted_left_press = Some(now);
                    }
                }
                0 => { // release
                    if suppress_current_left_click {
                        // Release matching a suppressed phantom press: drop it.
                        forward = false;
                        suppress_current_left_click = false;
                    } else {
                        // Do NOT forward yet. Wait the debounce window: if the
                        // faulty switch bounces back to pressed, we keep holding.
                        // All intervening events are buffered and replayed in order.
                        forward = false;
                        pending = Some(PendingRelease {
                            release_ev: ev,
                            deadline: Instant::now() + debounce,
                            buffered: Vec::new(),
                        });
                    }
                }
                2 => { // autorepeat should not happen for mouse buttons, but drop it if suppressed
                    if suppress_current_left_click {
                        forward = false;
                    }
                }
                _ => {}
            }
        }

        if forward {
            write_event(&mut uinput, &ev)?;
        } else if ev.type_ != EV_SYN && pending.is_none() {
            // Helpful when run from a terminal; comment out if too chatty.
            eprintln!("suppressed left-click event: value={}", ev.value);
        }
    }
}
