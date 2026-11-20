use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use codecraft_llm::{anthropic::Anthropic, claude_bridge::ClaudeBridge, openai_chat::OpenAiChat, Provider};
use codecraft_protocol::ThinkingLevel;
use serde::{Deserialize, Serialize};

use crate::approval::ApprovalMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    OpenaiChat,
    ClaudeCli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Environment variable holding the API key.
    #[serde(default)]
    pub env_key: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// For claude-cli: path to the `claude` binary.
    #[serde(default)]
    pub binary: Option<String>,
    /// Default context window for this provider's models.
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `provider/model`, e.g. `anthropic/claude-sonnet-4-5`, `claude/sonnet`, `ollama/qwen3`.
    pub model: String,
    pub approval: ApprovalMode,
    pub thinking: ThinkingLevel,
    pub max_tokens: u32,
    pub context_window: Option<u64>,
    /// Max sub-agent nesting. 1 = sub-agents cannot spawn sub-agents.
    pub subagent_depth: u32,
    /// `none` (default) or `firecracker`: run `bash` inside a microVM.
    pub sandbox: String,
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            approval: ApprovalMode::Ask,
            thinking: ThinkingLevel::Off,
            max_tokens: 16_000,
            context_window: None,
            subagent_depth: 1,
            sandbox: "none".into(),
            providers: BTreeMap::new(),
        }
    }
}

fn builtin_providers() -> BTreeMap<String, ProviderConfig> {
    let p = |kind, base: Option<&str>, env: Option<&str>, cw: Option<u64>| ProviderConfig {
        kind,
        base_url: base.map(String::from),
        env_key: env.map(String::from),
        api_key: None,
        binary: None,
        context_window: cw,
    };
    BTreeMap::from([
        ("anthropic".into(), p(ProviderKind::Anthropic, Some("https://api.anthropic.com/v1"), Some("ANTHROPIC_API_KEY"), Some(200_000))),
        ("openai".into(), p(ProviderKind::OpenaiChat, Some("https://api.openai.com/v1"), Some("OPENAI_API_KEY"), Some(272_000))),
        ("openrouter".into(), p(ProviderKind::OpenaiChat, Some("https://openrouter.ai/api/v1"), Some("OPENROUTER_API_KEY"), Some(128_000))),
        ("groq".into(), p(ProviderKind::OpenaiChat, Some("https://api.groq.com/openai/v1"), Some("GROQ_API_KEY"), Some(128_000))),
        ("ollama".into(), p(ProviderKind::OpenaiChat, Some("http://localhost:11434/v1"), None, Some(32_000))),
        ("claude".into(), p(ProviderKind::ClaudeCli, None, None, Some(200_000))),
    ])
}

pub fn home_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CODECRAFT_HOME") {
        return PathBuf::from(d);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".codecraft")
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

impl Config {
    /// Load `~/.codecraft/config.toml`, then `<project>/.codecraft/config.toml`.
    pub fn load(cwd: &Path) -> anyhow::Result<Config> {
        let mut v = toml::Value::Table(Default::default());
        let mut files = vec![home_dir().join("config.toml")];
        if let Some(root) = crate::context::project_root(cwd) {
            files.push(root.join(".codecraft/config.toml"));
        }
        for f in files {
            if let Ok(s) = std::fs::read_to_string(&f) {
                let t: toml::Value = toml::from_str(&s).with_context(|| format!("parsing {}", f.display()))?;
                merge(&mut v, t);
            }
        }
        let mut cfg: Config = v.try_into().context("invalid config")?;
        let mut providers = builtin_providers();
        providers.extend(std::mem::take(&mut cfg.providers));
        cfg.providers = providers;
        Ok(cfg)
    }

    /// Pick a default model from what is available.
    pub fn default_model(&self) -> String {
        if !self.model.is_empty() {
            return self.model.clone();
        }
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return "anthropic/claude-sonnet-4-5".into();
        }
        if which("claude").is_some() {
            return "claude/default".into();
        }
        if std::env::var("OPENAI_API_KEY").is_ok() {
            return "openai/gpt-5".into();
        }
        "ollama/qwen3".into()
    }

    pub fn split_model<'a>(&self, spec: &'a str) -> anyhow::Result<(&'a str, &'a str)> {
        spec.split_once('/').ok_or_else(|| anyhow!("model must be `provider/model`, got `{spec}`"))
    }

    pub fn context_window(&self, spec: &str) -> u64 {
        if let Some(c) = self.context_window {
            return c;
        }
        let provider = spec.split('/').next().unwrap_or("");
        self.providers.get(provider).and_then(|p| p.context_window).unwrap_or(128_000)
    }

    /// Build a fresh provider instance for `provider/model`. Returns the provider and the bare model id.
    pub fn provider(&self, spec: &str, cwd: &Path) -> anyhow::Result<(Arc<dyn Provider>, String)> {
        let (pid, model) = self.split_model(spec)?;
        let pc = self.providers.get(pid).ok_or_else(|| {
            anyhow!("unknown provider `{pid}`. Known: {}", self.providers.keys().cloned().collect::<Vec<_>>().join(", "))
        })?;
        let key = || -> Option<String> {
            pc.api_key.clone().or_else(|| pc.env_key.as_ref().and_then(|k| std::env::var(k).ok()))
        };
        let p: Arc<dyn Provider> = match pc.kind {
            ProviderKind::Anthropic => {
                let Some(k) = key() else {
                    bail!("provider `{pid}` needs an API key in ${}", pc.env_key.clone().unwrap_or_default())
                };
                Arc::new(Anthropic::new(pid, pc.base_url.clone().unwrap_or("https://api.anthropic.com/v1".into()), k))
            }
            ProviderKind::OpenaiChat => {
                let base = pc.base_url.clone().ok_or_else(|| anyhow!("provider `{pid}` needs base_url"))?;
                if pc.env_key.is_some() && key().is_none() {
                    bail!("provider `{pid}` needs an API key in ${}", pc.env_key.clone().unwrap_or_default());
                }
                Arc::new(OpenAiChat::new(pid, base, key()))
            }
            ProviderKind::ClaudeCli => {
                let bin = pc.binary.clone().unwrap_or_else(|| "claude".into());
                Arc::new(ClaudeBridge::new(pid, bin, cwd.to_path_buf()))
            }
        };
        Ok((p, model.to_string()))
    }
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file())
}
