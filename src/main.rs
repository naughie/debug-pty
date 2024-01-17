#![allow(unused, unused_mut)]

use libc::c_int;
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::pty::OpenptyResult;

use termios::Termios;

use dotenvy::Error as DotError;

use std::ffi::OsStr;
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};

struct Args {
    shell: String,
    mode: WriterMode,
}

impl Args {
    fn from_command_line() -> Option<Self> {
        let mut args = std::env::args().skip(1);

        let mut shell: Option<String> = None;
        let mut mode = WriterMode::String;

        while let Some(arg) = args.next() {
            if arg == "--shell" {
                if let Some(arg) = args.next() {
                    shell = Some(arg);
                } else {
                    break;
                }
            } else if arg == "--mod" {
                if let Some(arg) = args.next() {
                    if arg == "str" {
                        mode = WriterMode::String;
                    } else if arg == "bytes" {
                        mode = WriterMode::Bytes;
                    }
                } else {
                    break;
                }
            } else if arg == "--help" {
                print_help();
                return None;
            }
        }

        let shell = shell.unwrap_or("/bin/bash".to_string());
        Some(Self { shell, mode })
    }
}

fn print_help() {
    println!("cargo run [ -- [--shell SHELL] [--mod [str|bytes]] ]");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(args) = Args::from_command_line() {
        let OpenptyResult { master, slave } = open_pty()?;
        let mut term = termios::Termios::from_fd(master.as_raw_fd())?;
        debug_termios(&term);

        let env = match dotenvy::dotenv_iter() {
            Ok(env) => {
                let env: Result<Vec<_>, _> = env.collect();
                env?
            }
            Err(DotError::Io(e)) => {
                if matches!(e.kind(), IoErrorKind::NotFound) {
                    Vec::new()
                } else {
                    return Err(e.into());
                }
            }
            Err(e) => return Err(e.into()),
        };

        let mut cmd = build_cmd(&args.shell, slave.as_raw_fd(), env);

        let mut child = cmd.spawn()?;
        drop(slave);
        println!("Child PID {}", child.id());

        spawn_reader(master.as_raw_fd());

        write_loop(master.as_raw_fd(), args.mode)?;

        child.wait()?;

        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    Ok(())
}

fn open_pty() -> Result<OpenptyResult, Errno> {
    use nix::pty::openpty;

    let pty = openpty(None, None)?;
    fcntl(
        pty.master.as_raw_fd(),
        FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC),
    )?;
    fcntl(pty.slave.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;

    Ok(pty)
}

fn build_cmd(
    shell: impl AsRef<OsStr>,
    slave: RawFd,
    env: impl IntoIterator<Item = (String, String)>,
) -> Command {
    let mut cmd = Command::new(shell.as_ref());
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(slave))
            .stdout(Stdio::from_raw_fd(slave))
            .stderr(Stdio::from_raw_fd(slave))
            .pre_exec(move || {
                let res = libc::setsid();
                if res == -1 {
                    return Err(IoError::last_os_error());
                }

                let res = libc::ioctl(0, libc::TIOCSCTTY, 0);
                if res == -1 {
                    return Err(IoError::last_os_error());
                }

                Ok(())
            });
    }

    cmd.env_clear();
    cmd.env("SHELL", shell.as_ref());
    cmd.envs(env);

    cmd
}

fn spawn_reader(master: RawFd) {
    std::thread::spawn(move || {
        let mut buf = [0; 1024];
        loop {
            std::thread::sleep(std::time::Duration::from_millis(300));
            match nix::unistd::read(master, &mut buf) {
                Ok(num_bytes) => {
                    let buf = &buf[..num_bytes];
                    let buf_str = String::from_utf8_lossy(buf);
                    println!("READ");
                    println!("{buf_str:?}");
                    println!("{buf:02x?}");
                    println!();
                }
                Err(Errno::EIO) => {
                    println!("Got Errno::EIO");
                    break;
                }
                Err(e) => {
                    println!("Could not read the master: {e:?}");
                    break;
                }
            }
        }
    });
}

fn execute(cmd: &[u8], master: RawFd) -> Result<(), IoError> {
    println!("> {cmd:02x?}");
    if let Err(e) = nix::unistd::write(master, cmd) {
        println!("Error when writing to the master: {e:?}");
        Err(IoError::from_raw_os_error(e as _))
    } else {
        Ok(())
    }
}

enum WriterMode {
    String,
    Bytes,
}

fn write_loop(master: RawFd, mode: WriterMode) -> Result<(), IoError> {
    let stdin = std::io::stdin();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));

        let mut buf = String::new();
        stdin.read_line(&mut buf)?;

        let mut cmd = match mode {
            WriterMode::String => buf.into_bytes(),
            WriterMode::Bytes => parse_bytes(&buf),
        };

        if !cmd.ends_with(b"\n") {
            cmd.push(b'\n');
        }

        execute(&cmd, master.as_raw_fd())?;

        if cmd.ends_with(b"exit\n") {
            break;
        }
    }

    Ok(())
}

fn parse_bytes(buf: &str) -> Vec<u8> {
    let mut cmd = Vec::new();
    let buf = if let Some(buf) = buf.strip_suffix('\n') {
        buf
    } else {
        buf
    };

    for byte in buf.split(' ') {
        if let Ok(byte) = u8::from_str_radix(byte, 16) {
            cmd.push(byte);
        }
    }

    cmd
}

fn debug_termios(term: &Termios) {
    use ::termios::os::target::VSWTC as VSWTCH;
    use ::termios::os::target::*;
    use std::collections::BTreeMap;
    use std::fmt;

    macro_rules! flag_list {
        ($($flag: ident,)*) => {{
            [ $( ($flag, stringify!($flag)), )* ]
        }};
    }

    let iflags = flag_list![
        IGNBRK, BRKINT, IGNPAR, PARMRK, INPCK, ISTRIP, INLCR, ICRNL, IUCLC, IXON, IXANY, IXOFF,
        IMAXBEL, IUTF8,
    ];
    let oflags = flag_list![
        OPOST, OLCUC, ONLCR, OCRNL, ONOCR, ONLRET, OFILL, OFDEL, NLDLY, CRDLY, TABDLY, BSDLY,
        VTDLY, FFDLY,
    ];
    let cflags = flag_list![
        CBAUD, CBAUDEX, CSIZE, CSTOPB, CREAD, PARENB, PARODD, HUPCL, CLOCAL, CIBAUD, CMSPAR,
        CRTSCTS,
    ];
    let lflags = flag_list![
        ISIG, ICANON, XCASE, ECHO, ECHOE, ECHOK, ECHONL, ECHOCTL, ECHOPRT, ECHOKE, FLUSHO, NOFLSH,
        TOSTOP, PENDIN, IEXTEN,
    ];
    let cc = flag_list![
        VDISCARD, VEOF, VEOL, VEOL2, VERASE, VINTR, VKILL, VLNEXT, VMIN, VQUIT, VREPRINT, VSTART,
        VSTOP, VSUSP, VSWTCH, VTIME, VWERASE,
    ];

    #[derive(Debug)]
    struct Flag {
        r#in: Vec<&'static str>,
        out: Vec<&'static str>,
    }

    fn split(target: tcflag_t, flags: &[(tcflag_t, &'static str)]) -> Flag {
        let mut r#in = Vec::new();
        let mut out = Vec::new();

        for &(flag, name) in flags {
            if target & flag == 0 {
                out.push(name);
            } else {
                r#in.push(name);
            }
        }

        Flag { r#in, out }
    }

    struct Cc(BTreeMap<&'static str, cc_t>);

    fn special_char_map(target: &[cc_t], cc: &[(usize, &'static str)]) -> Cc {
        let mut map = BTreeMap::new();

        for &(c, name) in cc {
            map.insert(name, target[c]);
        }

        Cc(map)
    }

    struct DebugTermios {
        c_iflag: Flag,
        c_oflag: Flag,
        c_cflag: Flag,
        c_lflag: Flag,
        c_cc: Cc,
    }

    let new_dbg = || DebugTermios {
        c_iflag: split(term.c_iflag, &iflags),
        c_oflag: split(term.c_oflag, &oflags),
        c_cflag: split(term.c_cflag, &cflags),
        c_lflag: split(term.c_lflag, &lflags),
        c_cc: special_char_map(&term.c_cc, &cc),
    };

    impl fmt::Debug for DebugTermios {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Termios")
                .field("c_iflag", &self.c_iflag)
                .field("c_oflag", &self.c_oflag)
                .field("c_cflag", &self.c_cflag)
                .field("c_lflag", &self.c_lflag)
                .field("c_cc", &self.c_cc.0)
                .finish()
        }
    }

    println!("{:x?}", new_dbg());
}
