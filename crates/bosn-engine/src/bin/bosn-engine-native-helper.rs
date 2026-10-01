use std::{env, io::Write, time::Duration};

fn main() {
    match env::args().nth(2).as_deref() {
        Some("separate-130") => {
            println!("out");
            eprintln!("err");
            std::process::exit(130);
        }
        Some("sleep") => std::thread::sleep(Duration::from_secs(30)),
        Some("large") => print!("{}", "x".repeat(4096)),
        Some("large-sleep") => {
            print!("{}", "x".repeat(4096));
            std::io::stdout().flush().expect("flush");
            std::thread::sleep(Duration::from_secs(30));
        }
        Some("huge-sleep") => {
            print!("{}", "x".repeat(20_000));
            std::io::stdout().flush().expect("flush");
            std::thread::sleep(Duration::from_secs(30));
        }
        Some("stream") => {
            println!("first");
            std::io::stdout().flush().expect("flush");
            std::thread::sleep(Duration::from_secs(30));
        }
        Some("burst") => {
            // A compiler-warning flood: far more output than the event queue
            // holds, written as fast as the pipe accepts it, then exit 0.
            let line = "warning: duplicate compiler identity executed 2 times\n";
            let mut out = std::io::stdout().lock();
            for _ in 0..4_000 {
                out.write_all(line.as_bytes()).expect("write");
            }
            out.flush().expect("flush");
        }
        Some("short-sleep") => std::thread::sleep(Duration::from_millis(250)),
        Some("context") => {
            println!(
                "{}",
                env::current_dir().expect("current directory").display()
            );
            println!("{}", env::var("BOSN_ENGINE_TEST_ENV").unwrap_or_default());
        }
        #[cfg(unix)]
        Some("closed-pipes-sleep") => {
            // Test-only POSIX fixture: close both inherited pipe descriptors
            // while the direct child remains alive. Production uses only the
            // kernal-api facade for process and OS effects.
            unsafe {
                libc::close(libc::STDOUT_FILENO);
                libc::close(libc::STDERR_FILENO);
            }
            std::thread::sleep(Duration::from_secs(30));
        }
        _ => std::process::exit(2),
    }
}
