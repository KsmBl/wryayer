//! Running `wryayer` subcommands as child processes and streaming their output
//! into a plain GTK console window — the GUI equivalent of the TUI's
//! `launch_op`/`spawn_wryayer` pair. Supports a queue of jobs so several
//! installs can stream into one console back to back.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::glib;

pub enum OpMsg {
    Line(String),
    Done(bool),
}

/// Spawn `wryayer <args>` and stream its stdout+stderr, line by line, over the
/// returned channel. Terminates with `OpMsg::Done(success)`.
///
/// `stdin_data` is written to the child and its stdin then closed — that is how
/// an encrypting operation receives its passwords, rather than through argv
/// (world-readable via `/proc`) or the environment (inherited by every further
/// child, veracrypt included).
fn spawn(args: Vec<String>, stdin_data: Option<String>) -> Receiver<OpMsg> {
    let (tx, rx) = mpsc::channel();
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    thread::spawn(move || {
        let mut child = match Command::new(&exe)
            .args(&args)
            .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(OpMsg::Line(format!("error: {e}")));
                let _ = tx.send(OpMsg::Done(false));
                return;
            }
        };

        // Closing stdin is what tells the child's reader to stop waiting.
        if let Some(data) = stdin_data {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            }
        }

        let stderr = child.stderr.take().unwrap();
        let tx2 = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx2.send(OpMsg::Line(crate::child_output::sanitize_line(&line)));
            }
        });

        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(OpMsg::Line(crate::child_output::sanitize_line(&line)));
            }
        }

        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(OpMsg::Done(ok));
    });
    rx
}

/// One queued job: a separator label, the arguments, and anything to feed the
/// child on stdin.
struct Job {
    label: String,
    args: Vec<String>,
    stdin: Option<String>,
}

/// Open a console window and run a single `wryayer <args>` job, then call
/// `on_done(success)`.
pub fn run_operation<F>(parent: &gtk::ApplicationWindow, title: &str, args: Vec<String>, on_done: F)
where
    F: Fn(bool) + 'static,
{
    run_queue(parent, title, vec![Job { label: String::new(), args, stdin: None }], on_done);
}

/// Open a console window and run each job in `jobs` sequentially, streaming all
/// their output into the same view. Each job is `(label, args)`; a non-empty
/// label is printed as a separator header. `on_done(all_ok)` fires when done.
pub fn run_jobs<F>(
    parent: &gtk::ApplicationWindow,
    title: &str,
    jobs: Vec<(String, Vec<String>)>,
    on_done: F,
) where
    F: Fn(bool) + 'static,
{
    let jobs = jobs
        .into_iter()
        .map(|(label, args)| Job { label, args, stdin: None })
        .collect();
    run_queue(parent, title, jobs, on_done);
}

fn run_queue<F>(
    parent: &gtk::ApplicationWindow,
    title: &str,
    jobs: Vec<Job>,
    on_done: F,
) where
    F: Fn(bool) + 'static,
{
    let win = gtk::Window::builder()
        .title(title)
        .transient_for(parent)
        .modal(false)
        .default_width(680)
        .default_height(460)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(6);
    vbox.set_margin_end(6);

    let text = gtk::TextView::new();
    text.set_editable(false);
    text.set_monospace(true);
    text.set_cursor_visible(false);
    let buffer = text.buffer();

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&text));
    vbox.append(&scroller);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let status = gtk::Label::new(Some("Working…"));
    status.set_xalign(0.0);
    status.set_hexpand(true);
    let close = gtk::Button::with_label("Close");
    close.set_sensitive(false);
    bottom.append(&status);
    bottom.append(&close);
    vbox.append(&bottom);

    win.set_child(Some(&vbox));
    win.present();

    let win_weak = win.downgrade();
    close.connect_clicked(move |_| {
        if let Some(w) = win_weak.upgrade() {
            w.close();
        }
    });

    let multi = jobs.len() > 1;
    let jobs = Rc::new(RefCell::new(VecDeque::from(jobs)));
    let rx_cell: Rc<RefCell<Option<Receiver<OpMsg>>>> = Rc::new(RefCell::new(None));
    let overall_ok = Rc::new(Cell::new(true));
    let on_done = Rc::new(on_done);

    let append = {
        let buffer = buffer.clone();
        move |s: &str| {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, s);
            buffer.insert(&mut buffer.end_iter(), "\n");
        }
    };
    let start_next = {
        let jobs = jobs.clone();
        let rx_cell = rx_cell.clone();
        let append = append.clone();
        move || -> bool {
            if let Some(job) = jobs.borrow_mut().pop_front() {
                if multi && !job.label.is_empty() {
                    append(&format!("── {} ──", job.label));
                }
                *rx_cell.borrow_mut() = Some(spawn(job.args, job.stdin));
                true
            } else {
                false
            }
        }
    };
    start_next();

    glib::timeout_add_local(Duration::from_millis(40), move || {
        let mut finished_all = false;
        loop {
            let msg = rx_cell.borrow().as_ref().map(|rx| rx.try_recv());
            let Some(msg) = msg else {
                finished_all = true;
                break;
            };
            match msg {
                Ok(OpMsg::Line(line)) => append(&line),
                Ok(OpMsg::Done(ok)) => {
                    if !ok {
                        overall_ok.set(false);
                    }
                    *rx_cell.borrow_mut() = None;
                    if !start_next() {
                        finished_all = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    overall_ok.set(false);
                    *rx_cell.borrow_mut() = None;
                    if !start_next() {
                        finished_all = true;
                        break;
                    }
                }
            }
        }

        let mut end = buffer.end_iter();
        text.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);

        if finished_all {
            let ok = overall_ok.get();
            status.set_text(if ok { "Done." } else { "Failed." });
            close.set_sensitive(true);
            on_done(ok);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}
