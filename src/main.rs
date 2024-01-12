use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::pty::OpenptyResult;

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

        loop {
            match args.next() {
                Some(arg) => {
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
                None => break,
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

        let env = match dotenvy::dotenv_iter() {
            Ok(env) => {
                let env: Result<Vec<_>, _> = env.collect();
                let env = env?;
                env
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
            .pre_exec(|| {
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
                    println!("");
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
    let buf = if buf.ends_with('\n') {
        &buf[..(buf.len() - 1)]
    } else {
        &buf[..]
    };

    for byte in buf.split(' ') {
        if let Ok(byte) = u8::from_str_radix(byte, 16) {
            cmd.push(byte);
        }
    }

    cmd
}
