use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::ptr;
use std::slice;
use std::thread::sleep;
use std::time::{Duration, Instant};

// Linux input event constants. See /usr/include/linux/input-event-codes.h
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const SYN_REPORT: u16 = 0x00;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
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
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_5570;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

const INPUT_PROP_POINTER: i32 = 0x00;

const EVENT_SIZE: usize = size_of::<InputEvent>();

#[repr(C)]
#[derive(Copy, Clone)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// Equality deliberately ignores the timestamp: it is not part of a decision, and
/// forwarded events carry hardware timestamps while synthesised ones are zeroed.
impl PartialEq for InputEvent {
    fn eq(&self, other: &Self) -> bool {
        self.type_ == other.type_ && self.code == other.code && self.value == other.value
    }
}
impl Eq for InputEvent {}
impl std::fmt::Debug for InputEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ev(type={}, code={}, value={})", self.type_, self.code, self.value)
    }
}

impl InputEvent {
    /// Kernel stamps uinput writes with its own clock, so a zero timestamp is fine
    /// for events we synthesise; forwarded events keep the hardware timestamp.
    fn new(type_: u16, code: u16, value: i32) -> Self {
        InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        }
    }

    fn syn() -> Self {
        Self::new(EV_SYN, SYN_REPORT, 0)
    }
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

#[derive(Copy, Clone, Debug)]
struct Config {
    /// A press arriving this soon after an accepted press is contact bounce on the
    /// make edge (a phantom extra click) and is dropped, together with its release.
    press_window: Duration,
    /// How long a left release may be held back while we wait for break bounce.
    release_window: Duration,
    /// Only hold a release back if the button was logically down for at least this
    /// long. A short press with no movement is a plain click: its release is
    /// forwarded immediately and the pointer never notices anything.
    hold_threshold: Duration,
    verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            press_window: Duration::from_millis(80),
            release_window: Duration::from_millis(30),
            hold_threshold: Duration::from_millis(150),
            verbose: false,
        }
    }
}

/// What the caller must do with an incoming event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Cmd {
    /// Write it to the virtual device verbatim, right now.
    Pass,
    /// It was a bounce: drop it.
    Swallow,
}

/// Why an event was swallowed (diagnostics only).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Note {
    PhantomPress,
    PhantomRelease,
    BounceRepress,
    StrayRelease,
    HeldRelease,
}

/// The debounce state machine. Deliberately free of I/O and of `std::process`, so
/// the "motion is never delayed" invariant below is unit-testable.
#[derive(Debug)]
struct Filter {
    cfg: Config,
    last_press: Option<Instant>,
    /// Set when a press was swallowed as a phantom: swallow its release too.
    drop_next_release: bool,
    /// Logical BTN_LEFT state as far as the compositor knows.
    left_down: bool,
    /// Movement while the button was down => the user is dragging.
    moved_while_down: bool,
    press_at: Instant,
    /// A left release waiting out `release_window`; with its confirmation deadline.
    /// Only the release is held back — never motion, never the other buttons.
    held_release: Option<(InputEvent, Instant)>,
}

impl Filter {
    fn new(cfg: Config) -> Self {
        Filter {
            cfg,
            last_press: None,
            drop_next_release: false,
            left_down: false,
            moved_while_down: false,
            press_at: Instant::now(),
            held_release: None,
        }
    }

    fn on_event(&mut self, ev: &InputEvent, now: Instant) -> (Cmd, Option<Note>) {
        if ev.type_ == EV_KEY && ev.code == BTN_LEFT {
            return self.on_left_button(ev, now);
        }
        if self.left_down && ev.type_ == EV_REL && (ev.code == REL_X || ev.code == REL_Y) {
            self.moved_while_down = true;
        }
        // Everything that is not one of our two BTN_LEFT edges is forwarded the
        // instant it arrives: REL_X/REL_Y, wheel, other buttons, EV_SYN.
        (Cmd::Pass, None)
    }

    fn on_left_button(&mut self, ev: &InputEvent, now: Instant) -> (Cmd, Option<Note>) {
        match ev.value {
            1 => {
                // A press inside the release window is break-edge bounce: the button
                // never really let go. Swallow both edges, stay logically down.
                if self.held_release.take().is_some() {
                    self.left_down = true;
                    return (Cmd::Swallow, Some(Note::BounceRepress));
                }
                let phantom = self.cfg.press_window > Duration::ZERO
                    && self
                        .last_press
                        .is_some_and(|t| now.duration_since(t) < self.cfg.press_window);
                if phantom {
                    self.drop_next_release = true;
                    return (Cmd::Swallow, Some(Note::PhantomPress));
                }
                self.last_press = Some(now);
                self.press_at = now;
                self.left_down = true;
                self.moved_while_down = false;
                self.drop_next_release = false;
                (Cmd::Pass, None)
            }
            0 => {
                if self.drop_next_release {
                    self.drop_next_release = false;
                    return (Cmd::Swallow, Some(Note::PhantomRelease));
                }
                if !self.left_down {
                    // Release with no press behind it (e.g. after a phantom pair):
                    // the compositor already thinks the button is up.
                    return (Cmd::Swallow, Some(Note::StrayRelease));
                }
                self.left_down = false;
                let hold = self.cfg.release_window > Duration::ZERO
                    && (self.moved_while_down
                        || self.cfg.hold_threshold == Duration::ZERO
                        || now.duration_since(self.press_at) >= self.cfg.hold_threshold);
                if hold {
                    // Only the release waits. Motion keeps flowing while we decide.
                    self.held_release = Some((*ev, now + self.cfg.release_window));
                    return (Cmd::Swallow, Some(Note::HeldRelease));
                }
                (Cmd::Pass, None)
            }
            2 => {
                // Autorepeat should not happen for mouse buttons.
                if self.left_down {
                    (Cmd::Pass, None)
                } else {
                    (Cmd::Swallow, Some(Note::StrayRelease))
                }
            }
            _ => (Cmd::Pass, None),
        }
    }

    /// How long the caller should wait before re-checking the held release.
    /// `None` means "no timer pending, block on read forever".
    fn time_to_confirm(&self, now: Instant) -> Option<Duration> {
        let (_, deadline) = self.held_release?;
        Some(deadline.saturating_duration_since(now))
    }

    /// The held release, if its window has expired with no bounce.
    fn confirm(&mut self, now: Instant) -> Option<InputEvent> {
        let (ev, deadline) = self.held_release?;
        if now < deadline {
            return None;
        }
        self.held_release = None;
        Some(ev)
    }
}

/// If a held left release has aged out of its window, append it (plus the EV_SYN
/// that closes the frame) to `out`. A key event written without a following
/// SYN_REPORT is never handed to evdev readers at all -- the kernel only advances
/// `client->packet_head` on SYN_REPORT -- so an unterminated release would leave
/// the button looking stuck down until some later frame happened to close it.
fn append_confirmation(filter: &mut Filter, now: Instant, out: &mut Vec<u8>, verbose: bool) -> bool {
    if let Some(ev) = filter.confirm(now) {
        out.extend_from_slice(as_bytes(&ev));
        out.extend_from_slice(as_bytes(&InputEvent::syn()));
        if verbose {
            eprintln!("left release confirmed (button up)");
        }
        return true;
    }
    false
}

/// Write the whole filtered batch to the virtual device in one syscall:
/// /dev/uinput accepts a buffer holding many `struct input_event` records, so
/// writing event-by-event would cost one syscall per event at 1000 Hz for nothing.
/// /dev/uinput is blocking, so this waits if the compositor has not drained
/// UIO_MAX_QUEUE events -- that is honest backpressure, and the reason never to
/// open this fd with O_NONBLOCK (that would turn backpressure into lost events).
fn write_events<W: Write>(uinput: &mut W, bytes: &[u8]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    uinput.write_all(bytes)?;
    Ok(bytes.len() / EVENT_SIZE)
}

/// Take one read() worth of raw `struct input_event` records from the physical
/// device, filter them, and write the result to the virtual device in one write().
/// Split out from `main` so the byte-level behaviour is testable without a mouse.
fn process_bytes<W: Write>(
    uinput: &mut W,
    filter: &mut Filter,
    out: &mut Vec<u8>,
    data: &[u8],
    now: Instant,
    verbose: bool,
) -> io::Result<usize> {
    out.clear();
    out.reserve(data.len() + EVENT_SIZE * 2);
    for chunk in data.chunks_exact(EVENT_SIZE) {
        let ev = unsafe { ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
        let (cmd, note) = filter.on_event(&ev, now);
        if verbose {
            if let Some(nt) = note {
                eprintln!("swallowed {ev:?}: {nt:?}");
            }
        }
        if cmd == Cmd::Pass {
            out.extend_from_slice(as_bytes(&ev));
        }
    }
    // A deadline can expire in the middle of a batch. Emitting the held release at
    // the end of the batch keeps motion in its real order instead of replaying it
    // behind the button-up.
    append_confirmation(filter, now, out, verbose);
    write_events(uinput, out)
}

/// The poll-timeout path: no new events, so all there is to do is close out a
/// release whose window has expired.
fn flush_held<W: Write>(
    uinput: &mut W,
    filter: &mut Filter,
    out: &mut Vec<u8>,
    now: Instant,
    verbose: bool,
) -> io::Result<usize> {
    out.clear();
    append_confirmation(filter, now, out, verbose);
    write_events(uinput, out)
}

fn ioctl_int(fd: i32, request: libc::c_ulong, value: i32) -> io::Result<()> {
    let ret = unsafe { libc::ioctl(fd, request, value) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_noarg(fd: i32, request: libc::c_ulong) -> io::Result<()> {
    let ret = unsafe { libc::ioctl(fd, request) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn as_bytes<T>(p: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((p as *const T) as *const u8, size_of::<T>()) }
}

/// Wait up to `timeout` for `fd` to become readable. Ok(true) = data available.
fn wait_readable(fd: i32, timeout: Duration) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int;
    let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ret > 0)
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

    for rel in [
        REL_X,
        REL_Y,
        REL_WHEEL,
        REL_HWHEEL,
        REL_WHEEL_HI_RES,
        REL_HWHEEL_HI_RES,
    ] {
        ioctl_int(fd, UI_SET_RELBIT, rel as i32)?;
    }

    let _ = ioctl_int(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER);

    let mut uidev: UInputUserDev = unsafe { std::mem::zeroed() };
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
        "Usage: {program} /dev/input/eventX [debounce-ms] [options]\n\n\
Options:\n\
\x20 --press-ms=N     phantom re-press window after an accepted press (default 80)\n\
\x20 --release-ms=N   how long a left release may be held while dragging (default 30)\n\
\x20 --hold-ms=N      hold releases only once the button was down this long (default 150);\n\
\x20                  0 = always hold, which is what a click will also do\n\
\x20 --no-hold        never delay a release (only phantom presses are dropped)\n\
\x20 -v, --verbose    log every swallowed edge\n\n\
Example:\n\
\x20 sudo {program} /dev/input/by-id/usb-Your_Mouse-event-mouse 80\n\n\
Motion is never buffered, so the pointer stays fluid while a release is being\n\
debounced. Pick the physical mouse event node, not the virtual mouse this\n\
program creates."
    );
    std::process::exit(2);
}

/// `--opt=40` or `--opt 40`; exits with the usage text on anything unusable.
fn take_ms(prog: &str, inline: Option<&str>, argv: &[String], i: &mut usize) -> u64 {
    let raw: String = match inline {
        Some(v) => v.to_string(),
        None if *i < argv.len() => {
            let v = argv[*i].clone();
            *i += 1;
            v
        }
        None => usage(prog),
    };
    raw.parse::<u64>().unwrap_or_else(|_| usage(prog))
}

fn parse_args(prog: &str, argv: &[String]) -> (String, Config) {
    let mut cfg = Config::default();
    let mut path: Option<String> = None;
    let mut i = 0usize;
    while i < argv.len() {
        let arg = argv[i].as_str();
        i += 1;
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (arg, None),
        };
        match key {
            "-v" | "--verbose" => cfg.verbose = true,
            "--no-hold" => cfg.release_window = Duration::ZERO,
            "--press-ms" => {
                let ms = take_ms(prog, inline, argv, &mut i);
                cfg.press_window = Duration::from_millis(ms)
            }
            "--release-ms" => {
                let ms = take_ms(prog, inline, argv, &mut i);
                cfg.release_window = Duration::from_millis(ms)
            }
            "--hold-ms" => {
                let ms = take_ms(prog, inline, argv, &mut i);
                cfg.hold_threshold = Duration::from_millis(ms)
            }
            "-h" | "--help" => usage(prog),
            s if s.starts_with('-') => usage(prog),
            p => {
                if path.is_none() {
                    path = Some(p.to_string());
                } else if let Ok(ms) = p.parse::<u64>() {
                    // Legacy form: `prog /dev/input/eventX 80` = press window.
                    cfg.press_window = Duration::from_millis(ms);
                } else {
                    usage(prog);
                }
            }
        }
    }
    match path {
        Some(p) => (p, cfg),
        None => usage(prog),
    }
}

fn main() -> io::Result<()> {
    let all: Vec<String> = env::args().collect();
    let prog = all.first().map(String::as_str).unwrap_or("faulty-mouse-debounce");
    if all.len() < 2 {
        usage(prog);
    }
    let (input_path, cfg) = parse_args(prog, &all[1..]);

    if EVENT_SIZE != 24 {
        eprintln!(
            "warning: struct input_event is {} bytes here; this program assumes the \
             64-bit-time_t layout (24 bytes).",
            EVENT_SIZE
        );
    }

    let mut input = OpenOptions::new().read(true).open(&input_path)?;
    let input_fd = input.as_raw_fd();

    let mut uinput = create_virtual_mouse()?;
    let uinput_fd = uinput.as_raw_fd();

    let cleanup = || {
        let _ = ioctl_int(input_fd, EVIOCGRAB, 0);
        let _ = ioctl_noarg(uinput_fd, UI_DEV_DESTROY);
    };

    // Hide the broken physical mouse from the compositor. If this fails the
    // compositor keeps seeing the raw device too, which looks exactly like a
    // stuttering/jumping pointer — so make it loud rather than fatal.
    if let Err(e) = ioctl_int(input_fd, EVIOCGRAB, 1) {
        eprintln!(
            "warning: EVIOCGRAB on {input_path} failed ({e}).\n\
             \x20 The physical mouse is still visible to the desktop: expect doubled,\n\
             \x20 conflicting pointer events. Run as root to make the grab work."
        );
    }

    eprintln!(
        "Debouncing BTN_LEFT on {input_path}: press={}ms release={}ms hold>={}ms.\n\
         \x20 Pointer motion is forwarded immediately and never buffered.\n\
         \x20 Press Ctrl+C to stop.",
        cfg.press_window.as_millis(),
        cfg.release_window.as_millis(),
        cfg.hold_threshold.as_millis()
    );

    let mut filter = Filter::new(cfg);
    let mut buf = [0u8; EVENT_SIZE * 64];
    let mut out: Vec<u8> = Vec::with_capacity(buf.len() + EVENT_SIZE * 2);

    loop {
        // --- 1. While a release is on hold, wake up at its deadline; otherwise block.
        if let Some(remaining) = filter.time_to_confirm(Instant::now()) {
            if remaining.is_zero() {
                // Already due: nothing bounced, so the release was genuine.
                flush_held(&mut uinput, &mut filter, &mut out, Instant::now(), cfg.verbose)?;
                continue;
            }
            match wait_readable(input_fd, remaining) {
                Ok(true) => {}
                // Nothing arrived before the deadline: the release was genuine.
                Ok(false) => {
                    flush_held(&mut uinput, &mut filter, &mut out, Instant::now(), cfg.verbose)?;
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    cleanup();
                    return Err(e);
                }
            }
        }

        // --- 2. Drain everything the device has queued, in one syscall.
        let n = match input.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                cleanup();
                return Err(e);
            }
        };
        if n == 0 {
            eprintln!("{input_path} reached EOF (device unplugged?); exiting.");
            break;
        }
        if n % EVENT_SIZE != 0 {
            eprintln!("warning: short read of {n} bytes from {input_path}; trailing partial event ignored");
        }

        // --- 3. Filter the batch. Passing is immediate; only BTN_LEFT edges can
        // ever be swallowed or held. This also flushes a release whose window
        // expired in the middle of the batch.
        let now = Instant::now();
        process_bytes(&mut uinput, &mut filter, &mut out, &buf[..n], now, cfg.verbose)?;
    }

    flush_held(&mut uinput, &mut filter, &mut out, Instant::now(), false)?;
    cleanup();
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn press() -> InputEvent {
        InputEvent::new(EV_KEY, BTN_LEFT, 1)
    }
    fn release() -> InputEvent {
        InputEvent::new(EV_KEY, BTN_LEFT, 0)
    }
    fn move_x(dx: i32) -> InputEvent {
        InputEvent::new(EV_REL, REL_X, dx)
    }
    fn btn_right(v: i32) -> InputEvent {
        InputEvent::new(EV_KEY, BTN_RIGHT, v)
    }
    fn syn() -> InputEvent {
        InputEvent::new(EV_SYN, SYN_REPORT, 0)
    }

    fn decode(bytes: &[u8]) -> Vec<InputEvent> {
        bytes
            .chunks_exact(EVENT_SIZE)
            .map(|c| unsafe { ptr::read_unaligned(c.as_ptr() as *const InputEvent) })
            .collect()
    }

    /// Drives `main`'s own code path: raw `input_event` bytes in from the fake
    /// physical mouse, raw bytes out to the virtual device, via the same
    /// `process_bytes` / `flush_held` that `main` calls. No reimplementation.
    struct Sim {
        f: Filter,
        t: Instant,
        wire: Vec<u8>,
    }

    impl Sim {
        fn new(cfg: Config) -> Self {
            Sim {
                f: Filter::new(cfg),
                t: Instant::now(),
                wire: Vec::new(),
            }
        }

        /// Advance the fake clock.
        fn at(&mut self, ms: u64) {
            self.t += Duration::from_millis(ms);
        }

        /// One read() of the physical device: a batch of events, unfiltered bytes.
        fn feed(&mut self, evs: &[InputEvent]) {
            let mut raw = Vec::with_capacity(evs.len() * EVENT_SIZE);
            for e in evs {
                raw.extend_from_slice(as_bytes(e));
            }
            let mut out = Vec::new();
            process_bytes(&mut self.wire, &mut self.f, &mut out, &raw, self.t, false).unwrap();
        }

        /// A frame as a real device sends it: payload, then EV_SYN.
        fn frame(&mut self, evs: &[InputEvent]) {
            let mut batch = evs.to_vec();
            batch.push(syn());
            self.feed(&batch);
        }

        /// Time passing with nothing arriving from the device.
        fn tick(&mut self, ms: u64) {
            self.at(ms);
            self.feed(&[]);
        }

        fn out(&self) -> Vec<InputEvent> {
            decode(&self.wire)
        }

        fn held(&self) -> bool {
            self.f.held_release.is_some()
        }

        fn count(&self, ev: InputEvent) -> usize {
            self.out().iter().filter(|e| **e == ev).count()
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    /// The reported bug: a plain click must not hold anything back at all.
    #[test]
    fn plain_click_forwards_everything_immediately() {
        let mut s = Sim::new(cfg());
        s.frame(&[press()]);
        s.at(40);
        s.frame(&[release()]);
        assert!(!s.held(), "a short, still press must not open a hold window");
        assert_eq!(
            s.out(),
            vec![press(), syn(), release(), syn()],
            "the click is forwarded verbatim, both edges, nothing delayed"
        );
    }

    /// Motion, wheel and the other buttons flow while a left release is being
    /// debounced. This is the anti-stutter invariant.
    #[test]
    fn motion_is_never_buffered_while_a_release_is_held() {
        let mut s = Sim::new(cfg());
        s.frame(&[press()]);
        for _ in 0..2 {
            s.at(5);
            s.frame(&[move_x(-4)]); // dragging
        }
        s.at(5);
        s.frame(&[release()]); // -> the release alone waits
        assert!(s.held(), "a release during a drag is debounced");
        assert_eq!(s.count(release()), 0, "release not on the wire yet");

        for dx in [-4, -4, -3] {
            s.at(1);
            s.frame(&[move_x(dx)]);
            let out = s.out();
            assert_eq!(
                out.last(),
                Some(&syn()),
                "each motion frame is still closed by EV_SYN while the release is pending"
            );
            assert!(out.iter().any(|e| *e == move_x(dx)), "motion {dx} forwarded at once");
        }
        assert_eq!(
            s.out().iter().filter(|e| e.type_ == EV_REL).count(),
            5,
            "2 motion frames before the release + 3 after it, all delivered live"
        );

        s.tick(40); // window expires
        let out = s.out();
        let rel = out.iter().position(|e| *e == release()).expect("release flushed");
        assert_eq!(out[rel + 1], syn(), "a flushed key event must close its frame");
        // The motion that arrived during the window went out at the time it arrived,
        // so the flushed release must land *after* it, never cutting in front, and
        // never as a burst re-ordered behind the button-up.
        let last_motion = out
            .iter()
            .rposition(|e| e.type_ == EV_REL)
            .expect("motion on the wire");
        assert!(
            rel > last_motion,
            "release flushed at {rel} but the last motion was at {last_motion}: \
             motion must never be replayed behind the release"
        );
        assert_eq!(
            out[last_motion + 1],
            syn(),
            "the motion right before the flush kept its own frame"
        );
    }

    /// The original code wrote a flushed release with no EV_SYN of its own, which
    /// evdev withholds from readers until some later frame closes it.
    #[test]
    fn flush_on_a_still_mouse_still_closes_the_frame() {
        let mut f = Filter::new(Config {
            hold_threshold: Duration::ZERO, // always hold, worst case for framing
            ..cfg()
        });
        let t0 = Instant::now();
        f.on_event(&press(), t0);
        f.on_event(&release(), t0 + Duration::from_millis(200));
        let mut wire: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        flush_held(&mut wire, &mut f, &mut out, t0 + Duration::from_millis(210), false).unwrap();
        assert!(wire.is_empty(), "not due yet: nothing may be written");
        flush_held(&mut wire, &mut f, &mut out, t0 + Duration::from_millis(300), false).unwrap();
        assert_eq!(
            decode(&wire),
            vec![release(), syn()],
            "release + SYN_REPORT, and an empty buffer must not leave the release unterminated"
        );
    }

    /// Break-edge bounce while dragging: the button must stay logically down.
    #[test]
    fn bounce_while_dragging_keeps_the_button_down() {
        let mut s = Sim::new(cfg());
        s.frame(&[press()]);
        s.at(5);
        s.frame(&[move_x(-10)]);
        s.at(10);
        s.frame(&[release()]); // bounce: contact flickered open
        s.at(8);
        s.frame(&[press()]); // ... and closed again inside the window
        assert!(!s.held(), "the bounce cancelled the hold");
        assert_eq!(s.count(release()), 0, "a bouncing release never reaches the compositor");
        assert_eq!(s.count(press()), 1, "nor does the matching re-press");
        assert_eq!(s.count(move_x(-10)), 1, "but the drag motion kept flowing");

        s.at(60);
        s.frame(&[release()]); // the real release
        assert!(s.held());
        s.tick(40);
        assert_eq!(s.count(release()), 1, "exactly one release, at the end");
    }

    /// Phantom make-edge bounce (the classic double click) stays suppressed.
    #[test]
    fn phantom_repress_and_its_release_are_dropped() {
        let mut s = Sim::new(cfg());
        s.frame(&[press()]);
        s.at(30);
        s.frame(&[release()]);
        s.at(20); // 50 ms after the accepted press: inside the 80 ms press window
        s.frame(&[press()]);
        s.at(25);
        s.frame(&[release()]);
        assert_eq!(s.count(press()), 1, "one click, not two");
        assert_eq!(s.count(release()), 1);
        s.at(500);
        s.frame(&[press()]); // the next real click still works
        assert_eq!(s.count(press()), 2);
    }

    /// A long hold is still a hold: the release is debounced, so a drag that
    /// bounces is not cut into two.
    #[test]
    fn long_hold_gets_the_release_window() {
        let mut s = Sim::new(cfg());
        s.frame(&[press()]);
        s.at(400); // held 400 ms, no movement at all
        s.frame(&[release()]);
        assert!(s.held());
        s.tick(40);
        assert_eq!(s.count(release()), 1);
    }

    /// Nothing that is not a BTN_LEFT edge may ever be swallowed, delayed or
    /// reordered. Checked on a long pseudo-random stream, at the byte level.
    #[test]
    fn no_non_left_event_is_ever_lost() {
        let mut s = Sim::new(Config {
            hold_threshold: Duration::ZERO,
            release_window: Duration::from_millis(80),
            ..cfg()
        });
        let mut state: u64 = 0x243F6A88885A308D;
        let mut rng = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let mut sent: Vec<InputEvent> = Vec::new();
        for _ in 0..4000 {
            s.at((rng() % 12) as u64);
            let ev = match rng() % 10 {
                0 => press(),
                1 => release(),
                2 => btn_right((rng() % 2) as i32),
                n => move_x(n as i32 - 5),
            };
            let mut frame = vec![ev];
            if rng() % 4 == 0 {
                frame.push(btn_right((rng() % 2) as i32));
            }
            frame.push(syn());
            sent.extend(frame.iter().cloned());
            s.feed(&frame);
            s.tick(0);
        }
        // Non-SYN, non-left data events must pass through 1:1 and in order.
        // (We may inject an extra EV_SYN when re-emitting a held release, so SYNs
        // are not counted 1:1.)
        let is_data = |e: &&InputEvent| {
            e.type_ != EV_SYN && !(e.type_ == EV_KEY && e.code == BTN_LEFT)
        };
        let expected: Vec<InputEvent> = sent.iter().filter(is_data).cloned().collect();
        let got: Vec<InputEvent> = s.out().iter().filter(is_data).cloned().collect();
        assert_eq!(
            got.len(),
            expected.len(),
            "data events must pass through 1:1, never buffered or dropped"
        );
        assert_eq!(got, expected, "and in the exact order they arrived");
    }

    #[test]
    fn legacy_positional_and_flags_parse() {
        let v = |s: &str| s.to_string();
        let (path, c) = parse_args("p", &[v("/dev/input/event7"), v("80")]);
        assert_eq!(path, "/dev/input/event7");
        assert_eq!(c.press_window, Duration::from_millis(80));
        assert_eq!(c.release_window, Duration::from_millis(30), "new, much shorter");
        let (path, c) = parse_args(
            "p",
            &[v("-v"), v("/dev/x"), v("--release-ms=15"), v("--no-hold")],
        );
        assert_eq!(path, "/dev/x");
        assert!(c.verbose);
        assert_eq!(c.release_window, Duration::ZERO, "--no-hold disables the hold");
        let (_, c) = parse_args("p", &[v("/dev/x"), v("--hold-ms"), v("0")]);
        assert_eq!(c.hold_threshold, Duration::ZERO);
    }
    /// A `Write` sink that counts syscalls, to pin down "one write() per batch".
    #[derive(Default)]
    struct Counted {
        bytes: usize,
        writes: usize,
    }
    impl Write for Counted {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.bytes += b.len();
            self.writes += 1;
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The whole point of the coalescing: N events in => 1 write() out.
    #[test]
    fn one_read_batch_is_one_write() {
        let mut f = Filter::new(cfg());
        let mut sink = Counted::default();
        let mut out = Vec::new();
        let mut raw = Vec::new();
        for e in [press(), move_x(-3), syn(), move_x(-3), syn()] {
            raw.extend_from_slice(as_bytes(&e));
        }
        let n = process_bytes(&mut sink, &mut f, &mut out, &raw, Instant::now(), false).unwrap();
        assert_eq!(n, 5, "5 events forwarded");
        assert_eq!(sink.writes, 1, "one write() for the whole batch");
        assert_eq!(sink.bytes, 5 * EVENT_SIZE);
        // and an empty batch with nothing pending writes nothing at all
        assert_eq!(process_bytes(&mut sink, &mut f, &mut out, &[], Instant::now(), false).unwrap(), 0);
        assert_eq!(sink.writes, 1, "no spurious empty write()");
    }
}
