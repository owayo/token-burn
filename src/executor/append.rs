//! 実行開始時の設定を固定し、TUI から追加されたタスクにも同じ実行条件を使う。
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    ExecutionPlan, OutputPipeline,
    scripts::{TaskCtx, TaskResume, build_task_script},
};

#[cfg(test)]
mod tests;
use crate::{
    config::{Config, RuntimeAgent},
    resume::{ResumeDecision, ResumeStore},
    runtime::Session,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    command: Vec<String>,
    env: BTreeMap<String, String>,
    output: OutputPipeline,
    prompt: Result<String, String>,
    overrides: BTreeMap<PathBuf, Result<String, String>>,
    resume: bool,
    resume_prompt: Result<String, String>,
    peers: Vec<String>,
    global: bool,
    public_only: bool,
}

impl Settings {
    pub fn new(
        config: &Config,
        plan: &ExecutionPlan,
        resume: bool,
        peers: Vec<String>,
        global: bool,
        public_only: bool,
    ) -> Self {
        let resolve = |prompt: &str| {
            config
                .resolve_prompt(prompt)
                .map_err(|error| format!("{error:#}"))
        };
        Self {
            command: plan.agent.command.clone(),
            env: plan.agent.env.clone(),
            output: plan.output,
            prompt: resolve(
                plan.agent
                    .prompt
                    .as_deref()
                    .unwrap_or(&config.prompts.default),
            ),
            overrides: config
                .targets
                .iter()
                .filter_map(|target| {
                    Some((
                        {
                            let path = crate::config::resolve_directory(&target.directory).ok()?;
                            path.canonicalize().unwrap_or(path)
                        },
                        resolve(target.prompt.as_deref()?),
                    ))
                })
                .collect(),
            resume,
            resume_prompt: if plan.resume.prompt.is_empty() {
                config.resume_prompt().map_err(|error| format!("{error:#}"))
            } else {
                Ok(plan.resume.prompt.clone())
            },
            peers,
            global,
            public_only,
        }
    }
}

pub fn add(directory: &Path, session: &mut Session, path: &Path) -> Result<()> {
    anyhow::ensure!(
        crate::rate_control::now_epoch() < session.monitor.deadline_epoch,
        "The run deadline has passed"
    );
    anyhow::ensure!(
        !session.monitor.stop_file.try_exists()?,
        "The run is stopping; no targets can be added"
    );
    let settings: Settings = crate::runtime::read_json(&directory.join("append.json"))
        .context("This session does not support adding targets; start a new run")?;
    anyhow::ensure!(
        !settings.public_only,
        "Cannot add an unverified directory to a --public-only run"
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("Cannot open directory: {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "Not a directory: {}", path.display());
    anyhow::ensure!(
        !session
            .targets
            .iter()
            .any(|existing| existing.canonicalize().unwrap_or_else(|_| existing.clone()) == path),
        "This directory is already in the run"
    );
    let prompt = settings
        .overrides
        .get(&path)
        .unwrap_or(&settings.prompt)
        .clone()
        .map_err(anyhow::Error::msg)?;
    let target = crate::scanner::ResolvedTarget {
        display_name: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
        directory: path.clone(),
        prompt,
        visibility: crate::scanner::Visibility::Unknown,
        defer: false,
    };
    let agent = RuntimeAgent {
        name: session.monitor.agent.clone(),
        command: settings.command,
        env: settings.env,
        ..Default::default()
    };
    let store = ResumeStore::load_or_warn(&crate::resume::resume_path(&session.state_file));
    let state = crate::state::State::load(&session.state_file)?;
    let decision = store.get(&agent.name, &path).map(|entry| {
        if settings.resume {
            let last = state.last_processed_in_scope(&path, |name| {
                settings.global || settings.peers.iter().any(|peer| peer == name)
            });
            crate::resume::decide(entry, &target.prompt, last.as_ref())
        } else {
            ResumeDecision::Skip {
                session_id: entry.session_id.clone(),
                reason: crate::resume::SkipReason::Disabled("resume disabled for this run".into()),
            }
        }
    });
    let resumed = decision.as_ref().and_then(ResumeDecision::resume_entry);
    let discard = match &decision {
        Some(ResumeDecision::Skip { session_id, .. }) => Some(session_id.as_str()),
        _ => None,
    };
    let index = session
        .targets
        .len()
        .checked_add(1)
        .context("Too many targets")?;
    let prompt_file = directory.join(format!("prompt-{index}.txt"));
    let script_file = directory.join("tasks").join(format!("task-{index:04}.sh"));
    let pending = directory.join("queue").join(format!("pending-{index:04}"));
    let resume_prompt = directory.join("resume-prompt.txt");
    let exe = std::env::current_exe()?;
    let result = (|| -> Result<()> {
        fs::write(&prompt_file, &target.prompt)?;
        if resumed.is_some() && !resume_prompt.try_exists()? {
            fs::write(
                &resume_prompt,
                settings.resume_prompt.map_err(anyhow::Error::msg)?,
            )?;
        }
        let script = build_task_script(&TaskCtx {
            idx: index,
            total: index,
            task: &target,
            agent: &agent,
            prompt_file: &prompt_file,
            run_dir: &session.monitor.report_dir,
            marker_dir: &session.monitor.marker_dir,
            exe_path: &exe,
            state_file: &session.state_file,
            stop_file: &session.monitor.stop_file,
            rate_limit_threshold: session.monitor.threshold,
            output: settings.output,
            save_interrupted: session.save_interrupted,
            resume: resumed.map(|entry| TaskResume {
                entry,
                prompt_file: &resume_prompt,
            }),
            discard_session: discard,
        });
        fs::write(&script_file, script)?;
        if let Some(pause) = super::saved_pause(
            &resumed
                .into_iter()
                .filter(|entry| {
                    entry
                        .retry_after
                        .is_some_and(|at| at.timestamp() > crate::rate_control::now_epoch())
                })
                .collect::<Vec<_>>(),
        ) {
            crate::rate_control::write_pause(
                &crate::rate_control::pause_path_for(&session.monitor.stop_file),
                &pause,
            )?;
        }
        let previous = session.clone();
        session.targets.push(path);
        session.monitor.total = index;
        let publish = (|| -> Result<()> {
            // pending が見える時点で、再接続先とモニターには新しい総数が公開済み。
            crate::runtime::atomic_json(&directory.join("session.json"), session)?;
            let marker = tempfile::NamedTempFile::new_in(directory.join("queue"))?;
            marker.persist(&pending)?;
            Ok(())
        })();
        if let Err(error) = publish {
            *session = previous;
            crate::runtime::atomic_json(&directory.join("session.json"), session)?;
            return Err(error);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(prompt_file);
        let _ = fs::remove_file(script_file);
    }
    result
}
