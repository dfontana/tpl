use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{ErrorLevel, Level, Verbosity};
use directories::{ProjectDirs, UserDirs};
use minijinja::value::Object;
use minijinja::{Environment, Value};
use notify_debouncer_full::notify::{Error, INotifyWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebouncedEvent, Debouncer, FileIdMap};
use parking_lot::{deadlock, Mutex, RwLock};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use std::{env, fs, thread};
use toml;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None, args_conflicts_with_subcommands(true))]
struct Cli {
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[command(flatten)]
    verbose: Verbosity<ErrorLevel>,
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(global(true), last(true), value_parser = parse_key_val::<String, String>)]
    vargs: Vec<(String, String)>,
}

fn parse_key_val<T, U>(
    s: &str,
) -> Result<(T, U), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
    U: std::str::FromStr,
    U::Err: std::error::Error + Send + Sync + 'static,
{
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid KEY=value: no `=` found in `{s}`"))?;
    Ok((s[..pos].parse()?, s[pos + 1..].parse()?))
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// continually run templating, reloading changes live as they are found
    Watch {
        /// How long to wait for file changes to take effect
        #[arg(short, long, value_parser = humantime::parse_duration, default_value="500ms")]
        debounce: Duration,
    },
}

#[derive(Serialize, Deserialize)]
struct Config {
    tpls: Vec<Tpl>,
    #[serde(flatten)]
    extra: HashMap<String, toml::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Tpl {
    // TODO: Code assumes files, perhaps future update allows directories (src && dst == dirs, relative copies)
    #[serde(deserialize_with = "resolve_tilde_s")]
    src: PathBuf,
    #[serde(deserialize_with = "resolve_tilde_s")]
    dst: PathBuf,
}

const APP_NAME: &str = env!("CARGO_PKG_NAME");
static HOME_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| UserDirs::new().unwrap().home_dir().to_path_buf());
static CONFIG: RwLock<Option<Config>> = RwLock::new(None);

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .filter_level(cli.verbose.log_level_filter())
        .init();

    if cli
        .verbose
        .log_level()
        .filter(|l| *l >= Level::Debug)
        .is_some()
    {
        spawn_deadlock_debug();
    }

    let cfg_loc = init_cfg_loc(&cli)?;
    load_config(&cfg_loc)?;

    let mut vargs = HashMap::new();
    for (k, v) in cli.vargs.iter() {
        if vargs.contains_key(k) {
            bail!("Ambiguous key in Cli args: {}", k);
        }
        vargs.insert(k.to_owned(), v.to_owned());
    }

    let mut env = Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);

    // Note: resolver will work with prefixes, while magic will not
    let arc_vargs = Arc::new(vargs);
    let ctx = MagicContext(Arc::new(HiddenCtx {
        env_res: Arc::new(EnvResolver),
        cfg_res: Arc::new(ConfigResolver),
        cli_res: Arc::new(CliResolver {
            vargs: arc_vargs.clone(),
        }),
    }));
    env.add_global("env", Value::from_object(EnvResolver));
    env.add_global("cfg", Value::from_object(ConfigResolver));
    env.add_global("cli", Value::from_object(CliResolver { vargs: arc_vargs }));

    let env_ref = Arc::new(env);

    let mut _watchers = None;
    match cli.command {
        Some(Commands::Watch { debounce }) => {
            _watchers = Some(init_watcher(&cfg_loc, debounce, env_ref, ctx)?);
            wait_for_ctrl_c();
        }
        None => {
            render_all(env_ref, ctx)?;
        }
    }

    Ok(())
}

/// Resolves a value from the environment
#[derive(Debug)]
struct EnvResolver;
impl Object for EnvResolver {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        std::env::var(key.as_str()?.to_uppercase())
            .map(Value::from)
            .ok()
            .inspect(|_| log::debug!("Resolved key from Env: {}", key))
    }
}

/// Resolves a value from the user's config
#[derive(Debug)]
struct ConfigResolver;
impl Object for ConfigResolver {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        log::debug!("Attempting config resolver: {}", key);
        CONFIG
            .read()
            .as_ref()
            .unwrap()
            .extra
            .get(key.as_str()?)
            .map(Value::from_serialize)
            .inspect(|_| log::debug!("Resolved key from Config: {}", key))
    }
}

/// Resolves a value from the given CLI params
#[derive(Debug)]
struct CliResolver {
    vargs: Arc<HashMap<String, String>>,
}
impl Object for CliResolver {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        self.vargs
            .get(key.as_str()?)
            .map(Value::from)
            .inspect(|_| log::debug!("Resolved key from Cli: {}", key))
    }
}

/// Resolves a value by attempting all other resolution methods in sequence.
/// Priority goes: Cli -> Env -> Config. The first to match will return.
#[derive(Clone, Debug)]
struct MagicContext(Arc<HiddenCtx>);
#[derive(Debug)]
struct HiddenCtx {
    env_res: Arc<EnvResolver>,
    cfg_res: Arc<ConfigResolver>,
    cli_res: Arc<CliResolver>,
}
impl Object for MagicContext {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        log::debug!("MagicContext: Looking for key: {:?}", key);
        let res = self
            .0
            .cli_res
            .get_value(key)
            .or_else(|| self.0.env_res.get_value(key))
            .or_else(|| self.0.cfg_res.get_value(key));
        if res.is_none() {
            log::error!("MagicContext: Cannot find value for variable: {}", key);
            return None;
        }
        res.inspect(|_| log::debug!("Resolved in MagicContext: {}", key))
    }
}

fn resolve_tilde_s<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    let s: PathBuf = Deserialize::deserialize(deserializer)?;
    resolve_tilde(&s).map_err(serde::de::Error::custom)
}

fn resolve_tilde(path: &PathBuf) -> Result<PathBuf, anyhow::Error> {
    if path.starts_with("~") {
        return Ok(HOME_DIR.join(path.strip_prefix("~")?));
    } else if !path.is_absolute() {
        bail!("Config must be an absolute path, or relative to home (~)")
    }
    return Ok(path.clone());
}

impl Tpl {
    fn src(&self) -> &Path {
        &self.src
    }

    /// Subscribe this template's src for updates. The parent is subscribed and later
    /// child events filtered to this template specifically, to cover a shortcoming of
    /// inotify where file deletes->creations cause subscriptions to be lost
    fn try_subscribe(&self, w: &mut INotifyWatcher) {
        println!("Subscribing to: {:?}", self.src());
        if let Err(err) = w
            .watch(&self.src().parent().unwrap(), RecursiveMode::Recursive)
            .with_context(|| format!("at path {:?}", self.src()))
        {
            log::error!("Subscription failed {:#}", err);
        }
    }

    fn render(&self, env: &Environment, ctx: &MagicContext) -> Result<(), anyhow::Error> {
        // TODO: I wonder if I have to buffer to string so much or if we can carry a buffer
        fs::write(
            &self.dst,
            env.render_str(
                &fs::read_to_string(self.src())?,
                Value::from_object(ctx.clone()),
            )?,
        )?;
        println!("Rendering: {:?}", self.dst);
        Ok(())
    }
}

fn render_all(env: Arc<Environment<'static>>, ctx: MagicContext) -> Result<(), anyhow::Error> {
    let binding = CONFIG.read();
    for tpl in binding.as_ref().unwrap().tpls.iter() {
        tpl.render(&env, &ctx)?;
    }
    Ok(())
}

fn wait_for_ctrl_c() {
    let (tx, rx) = channel();
    ctrlc::set_handler(move || tx.send(()).expect("Could not send signal on channel."))
        .expect("Error setting Ctrl-C handler");
    println!("Waiting for Ctrl-C...");
    rx.recv().expect("Could not receive from channel.");
}

fn spawn_deadlock_debug() {
    log::debug!("Spawning deadlock detector");
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(2));
        let deadlocks = deadlock::check_deadlock();
        if deadlocks.is_empty() {
            continue;
        }
        log::debug!("{} deadlocks detected", deadlocks.len());
        for (i, threads) in deadlocks.iter().enumerate() {
            log::debug!("Deadlock #{}", i);
            for t in threads {
                log::debug!("Thread Id {:#?}", t.thread_id());
                log::debug!("{:#?}", t.backtrace());
            }
        }
    });
}

fn init_cfg_loc(cli: &Cli) -> Result<PathBuf, anyhow::Error> {
    if let Some(pth) = &cli.config {
        return resolve_tilde(pth);
    }
    if let Some(dirs) = ProjectDirs::from("", APP_NAME, APP_NAME) {
        let cfg_dir = dirs.config_dir();
        if !cfg_dir.exists() {
            std::fs::create_dir(&cfg_dir)?;
        }
        if !cfg_dir.is_dir() {
            bail!(
                "Config directory is actually a file, please remove {:?}",
                cfg_dir
            );
        }
        let cfg_file = cfg_dir.join("config.toml");
        if !cfg_file.exists() {
            let mut file = std::fs::File::create_new(&cfg_file)?;
            writeln!(file, "{}", "")?;
        }
        if !cfg_file.is_file() {
            bail!("Config file is not a file, please remove {:?}", cfg_file);
        }
        return Ok(cfg_file);
    }
    bail!("Could not resolve user home directory")
}

fn load_config(cfg_loc: &Path) -> Result<(), anyhow::Error> {
    let new_cfg: Config = toml::from_str(
        &std::fs::read_to_string(cfg_loc).with_context(|| format!("at path {:?}", cfg_loc))?,
    )
    .with_context(|| format!("at path {:?}", cfg_loc))?;
    let mut cfg = CONFIG.write();
    *cfg = Some(new_cfg);
    Ok(())
}

fn init_watcher(
    cfg_loc: &Path,
    debounce: Duration,
    env: Arc<Environment<'static>>,
    ctx: MagicContext,
) -> Result<
    (
        Debouncer<INotifyWatcher, FileIdMap>,
        Arc<Mutex<Debouncer<INotifyWatcher, FileIdMap>>>,
    ),
    anyhow::Error,
> {
    // TODO: Nice to have: pretty printing subscribe, unsubscribe

    let env1 = env.clone();
    let ctx1 = ctx.clone();
    let watcher = Arc::new(Mutex::new(new_debouncer(
        debounce,
        None,
        move |res: Result<Vec<DebouncedEvent>, Vec<Error>>| match res {
            // TODO: Nice to have: re-render the precise files that changed only
            Ok(e) => {
                let binding = CONFIG.read();
                let cfg = binding.as_ref().unwrap();
                for e in e
                    .iter()
                    .filter(|e| e.kind.is_modify() || e.kind.is_create())
                {
                    for tpl in e
                        .paths
                        .iter()
                        .filter_map(|p| cfg.tpls.iter().find(|s| p == s.src()))
                    {
                        if let Err(e) = tpl.render(&env1, &ctx1) {
                            log::error!("Failed to render template: {:#}", e);
                        }
                    }
                }
            }
            Err(e) => log::error!("Watcher failed {:?}", e),
        },
    )?));

    {
        let mut w = watcher.lock();
        let binding = CONFIG.read();
        let cfg = binding.as_ref().unwrap();
        for t in cfg.tpls.iter() {
            t.try_subscribe(w.watcher());
            if let Err(e) = t.render(&env, &ctx) {
                log::error!("Failed to render template: {:#}", e);
            }
        }
    }
    let w2 = watcher.clone();

    // Note: inotify will unsubscribe from files that no longer exist. Some editors will delete then create
    // ergo, we actually need to watch the parent directory and filter for the file we want
    let cfg_loc_2 = cfg_loc.to_path_buf();
    let env2 = env.clone();
    let ctx2 = ctx.clone();
    let mut config_watcher = new_debouncer(
        debounce,
        None,
        move |res: Result<Vec<DebouncedEvent>, Vec<Error>>| match res {
            Ok(events) => {
                for _ in events
                    .iter()
                    .filter(|e| e.kind.is_modify() || e.kind.is_create())
                    .filter(|e| e.paths.contains(&cfg_loc_2))
                {
                    if let Err(e) = load_config(&cfg_loc_2) {
                        log::error!("Failed to load config {:#}", e);
                        return;
                    }
                    let binding = CONFIG.read();
                    let cfg = binding.as_ref().unwrap();
                    for t in cfg.tpls.iter() {
                        t.try_subscribe(w2.lock().watcher());
                        if let Err(e) = t.render(&env2, &ctx2) {
                            log::error!("Failed to render template: {:#}", e);
                        }
                    }
                }
            }
            Err(e) => log::error!("Watcher failed {:?}", e),
        },
    )?;
    config_watcher
        .watcher()
        .watch(cfg_loc.parent().unwrap(), RecursiveMode::Recursive)?;

    Ok((config_watcher, watcher))
}
