//! Scheduling and lifecycle callbacks from scripts (`nsd/tclsched.c`,
//! `nsd/tclcallbacks.c`).

use std::sync::Arc;
use std::time::Duration;

use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{bool_str, check_args, cmd, int_arg, server};
use crate::callbacks::When;
use crate::sched::{Repeat, SchedFlags};
use crate::server::Server;

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_schedule_proc", cmd(ns_schedule_proc)),
        ("ns_schedule_daily", cmd(ns_schedule_daily)),
        ("ns_schedule_weekly", cmd(ns_schedule_weekly)),
        ("ns_after", cmd(ns_after)),
        ("ns_cancel", cmd(ns_cancel)),
        ("ns_unschedule_proc", cmd(ns_cancel)),
        ("ns_pause", cmd(|i, a| pause(i, a, true))),
        ("ns_resume", cmd(|i, a| pause(i, a, false))),
        ("ns_atshutdown", cmd(|i, a| at(i, a, When::Shutdown))),
        ("ns_atexit", cmd(|i, a| at(i, a, When::Exit))),
        ("ns_atsignal", cmd(|i, a| at(i, a, When::Signal))),
        ("ns_atstartup", cmd(|i, a| at(i, a, When::Startup))),
        ("ns_atprestartup", cmd(|i, a| at(i, a, When::PreStartup))),
        ("ns_atready", cmd(|i, a| at(i, a, When::Ready))),
    ]
}

fn flags(argv: &[String]) -> (SchedFlags, usize) {
    let mut f = SchedFlags::default();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "-once" => f.once = true,
            "-thread" => f.thread = true,
            _ => break,
        }
        i += 1;
    }
    (f, i)
}

fn script_of(interp: &dyn Interp, rest: &[String]) -> String {
    if rest.len() == 1 {
        rest[0].clone()
    } else {
        interp.list_from(rest)
    }
}

fn schedule(srv: &Arc<Server>, script: String, f: SchedFlags, repeat: Repeat) -> u64 {
    let s = Arc::clone(srv);
    let desc = script.clone();
    srv.global.sched.schedule(
        Arc::new(move |_id| {
            let _ = crate::interp::eval_logged(&s, &script);
        }),
        f,
        repeat,
        &desc,
    )
}

fn ns_schedule_proc(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (f, i) = flags(argv);
    if argv.len() < i + 2 {
        return Err(wrong_args(
            argv,
            1,
            "?-once? ?-thread? interval script ?arg?",
        ));
    }
    let interval = int_arg(&argv[i])?;
    if interval < 0 {
        return Err(Exception::error(format!("invalid interval: {interval}")));
    }
    let srv = server(interp)?;
    let script = script_of(interp, &argv[i + 1..]);
    Ok(schedule(
        &srv,
        script,
        f,
        Repeat::Interval(Duration::from_secs(interval as u64)),
    )
    .to_string())
}

fn hm(h: &str, m: &str) -> Result<(u32, u32), Exception> {
    let h = int_arg(h)?;
    let m = int_arg(m)?;
    if !(0..=23).contains(&h) {
        return Err(Exception::error(format!("invalid hour: {h}")));
    }
    if !(0..=59).contains(&m) {
        return Err(Exception::error(format!("invalid minute: {m}")));
    }
    Ok((h as u32, m as u32))
}

fn ns_schedule_daily(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (f, i) = flags(argv);
    if argv.len() < i + 3 {
        return Err(wrong_args(
            argv,
            1,
            "?-once? ?-thread? hour minute script ?arg?",
        ));
    }
    let (hour, minute) = hm(&argv[i], &argv[i + 1])?;
    let srv = server(interp)?;
    let script = script_of(interp, &argv[i + 2..]);
    Ok(schedule(&srv, script, f, Repeat::Daily { hour, minute }).to_string())
}

fn ns_schedule_weekly(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (f, i) = flags(argv);
    if argv.len() < i + 4 {
        return Err(wrong_args(
            argv,
            1,
            "?-once? ?-thread? day hour minute script ?arg?",
        ));
    }
    let day = int_arg(&argv[i])?;
    if !(0..=6).contains(&day) {
        return Err(Exception::error(format!("invalid day: {day}")));
    }
    let (hour, minute) = hm(&argv[i + 1], &argv[i + 2])?;
    let srv = server(interp)?;
    let script = script_of(interp, &argv[i + 3..]);
    Ok(schedule(
        &srv,
        script,
        f,
        Repeat::Weekly {
            day: day as u32,
            hour,
            minute,
        },
    )
    .to_string())
}

fn ns_after(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, usize::MAX, 1, "seconds script ?arg?")?;
    let secs = int_arg(&argv[1])?.max(0) as u64;
    let srv = server(interp)?;
    let script = script_of(interp, &argv[2..]);
    let f = SchedFlags {
        once: true,
        thread: false,
    };
    Ok(schedule(
        &srv,
        script,
        f,
        Repeat::Interval(Duration::from_secs(secs.max(1))),
    )
    .to_string())
}

fn ns_cancel(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "id")?;
    let id = int_arg(&argv[1])?.max(0) as u64;
    Ok(bool_str(server(interp)?.global.sched.cancel(id)))
}

fn pause(interp: &dyn Interp, argv: &[String], paused: bool) -> ScriptResult {
    check_args(argv, 2, 2, 1, "id")?;
    let id = int_arg(&argv[1])?.max(0) as u64;
    Ok(bool_str(
        server(interp)?.global.sched.set_paused(id, paused),
    ))
}

fn at(interp: &dyn Interp, argv: &[String], when: When) -> ScriptResult {
    check_args(argv, 2, usize::MAX, 1, "script ?arg?")?;
    let srv = server(interp)?;
    let script = script_of(interp, &argv[1..]);
    let s = Arc::clone(&srv);
    srv.global.callbacks.register(
        when,
        Arc::new(move || {
            let _ = crate::interp::eval_logged(&s, &script);
        }),
    );
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_minute_validation() {
        assert_eq!(hm("23", "59"), Ok((23, 59)));
        assert!(hm("24", "0").is_err());
        assert!(hm("1", "60").is_err());
        let argv: Vec<String> = ["x", "-once", "-thread", "5"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let (f, i) = flags(&argv);
        assert!(f.once && f.thread);
        assert_eq!(i, 3);
    }
}
