use super::{
  argparse::Mode,
  subprocess::{stream_into, SubprocessCommand},
  types::{Abort, Fail},
};
use futures::future::try_join;
use std::{
  collections::HashMap,
  env::{self, current_exe},
  path::PathBuf,
  process::Stdio,
  sync::Arc,
};
use tokio::{
  io::{AsyncWriteExt, BufWriter, ErrorKind},
  process::Command,
  select,
  sync::mpsc::Receiver,
  task::{spawn, JoinHandle},
};
use which::which;

async fn reset_term() -> Result<(), Fail> {
  if let Ok(path) = which("tput") {
    let status = Command::new(&path)
      .kill_on_drop(true)
      .stdin(Stdio::null())
      .arg("reset")
      .status()
      .await
      .map_err(|e| Fail::IO(path, e.kind()))?;

    if status.success() {
      return Ok(());
    }
  }
  if let Ok(path) = which("reset") {
    let status = Command::new(&path)
      .kill_on_drop(true)
      .stdin(Stdio::null())
      .status()
      .await
      .map_err(|e| Fail::IO(path, e.kind()))?;
    if status.success() {
      return Ok(());
    }
  }
  Err(Fail::IO(PathBuf::from("reset"), ErrorKind::NotFound))
}

fn run_fzf(abort: &Arc<Abort>, cmd: SubprocessCommand, stream: Receiver<String>) -> JoinHandle<()> {
  let abort = abort.clone();

  spawn(async move {
    let subprocess = Command::new(&cmd.prog)
      .kill_on_drop(true)
      .args(&cmd.args)
      .envs(&cmd.env)
      .stdin(Stdio::piped())
      .spawn();

    match subprocess {
      Err(err) => {
        abort.send(Fail::IO(cmd.prog, err.kind())).await;
      }
      Ok(mut child) => {
        let mut stdin = child.stdin.take().map(BufWriter::new).expect("nil stdin");

        let abort_1 = abort.clone();
        let p1 = cmd.prog.clone();
        let handle_in = spawn(async move {
          stream_into(&abort_1, p1.clone(), &mut stdin, stream).await;
          if let Err(err) = stdin.shutdown().await {
            abort_1.send(Fail::IO(p1, err.kind())).await;
          }
        });

        let abort_2 = abort.clone();
        let p2 = cmd.prog.clone();
        let handle_child = spawn(async move {
          select! {
            _ = abort_2.notified() => {
              match child.kill().await {
                Err(err) => {
                  abort_2.send(Fail::IO(p2, err.kind())).await;
                },
                _ => {
                  if let Err(err) = reset_term().await {
                    abort_2.send(err).await;
                  }
                }
              }
            },
            rhs = child.wait() => {
              match rhs {
                Ok(status) => {
                  match status.code() {
                    Some(0) | Some(1) | None => (),
                    Some(130) => {
                      abort_2.send(Fail::Interrupt).await;
                    }
                    Some(c) => {
                      abort_2.send(Fail::BadExit(p2, c)).await;
                      if let Err(err) = reset_term().await {
                        abort_2.send(err).await;
                      }
                    }
                  }
                }
                Err(err) => {
                  abort_2.send(Fail::IO(p2, err.kind())).await;
                }
              }
            }
          }
        });

        if let Err(err) = try_join(handle_child, handle_in).await {
          abort.send(err.into()).await;
        }
      }
    }
  })
}

pub fn stream_fzf(
  abort: &Arc<Abort>,
  bin: PathBuf,
  args: Vec<String>,
  stream: Receiver<String>,
) -> JoinHandle<()> {
  let execute = format!("abort+execute:{}\x04{{+f}}", Mode::PATCH);
  let mut arguments = vec![
    "--read0".to_owned(),
    "--print0".to_owned(),
    "-m".to_owned(),
    "--ansi".to_owned(),
    "--preview-window=70%:wrap".to_owned(),
    format!("--bind=enter:{execute}"),
    format!("--bind=double-click:{execute}"),
    format!("--preview={}\x04{{f}}", Mode::PREVIEW),
  ];
  arguments.extend(args);

  let mut fzf_env = HashMap::new();
  fzf_env.insert(
    Mode::ARGV.to_owned(),
    env::args().collect::<Vec<_>>().join("\x04"),
  );
  fzf_env.insert(
    "SHELL".to_owned(),
    current_exe()
      .or_else(|_| which("sad"))
      .map(|path| format!("{}", path.display()))
      .unwrap_or_else(|_| "sad".to_owned()),
  );
  fzf_env.insert("LC_ALL".to_owned(), "C".to_owned());

  let cmd = SubprocessCommand {
    prog: bin,
    args: arguments,
    env: fzf_env,
  };
  run_fzf(abort, cmd, stream)
}
