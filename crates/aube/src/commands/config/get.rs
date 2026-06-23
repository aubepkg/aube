use super::{
    ListLocation, read_merged, read_single, resolve_aliases, setting_default_value,
    setting_for_key, user_npmrc_path,
};
use clap::Args;

#[derive(Debug, Args)]
pub struct GetArgs {
    /// The setting key.
    ///
    /// Accepts either a pnpm canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Emit the value as JSON.
    ///
    /// Matches `pnpm config get --json`: a missing key renders as
    /// `undefined`, a found value is JSON-encoded.
    #[arg(long)]
    pub json: bool,

    /// Shortcut for `--location project`.
    #[arg(long, conflicts_with = "location")]
    pub local: bool,

    /// Which config location(s) to read.
    ///
    /// Defaults to `merged` — the last-write-wins view of user aube
    /// config, `~/.npmrc`, then `./.npmrc`, matching what install
    /// actually sees. Use `user` or `project` to restrict the lookup.
    #[arg(long, value_enum, default_value_t = ListLocation::Merged)]
    pub location: ListLocation,
}

impl GetArgs {
    fn effective_location(&self) -> ListLocation {
        if self.local {
            ListLocation::Project
        } else {
            self.location
        }
    }
}

pub fn run(args: GetArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    let cwd = crate::dirs::project_root_or_cwd()?;
    let entries: Vec<(String, String)> = match args.effective_location() {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User | ListLocation::Global => {
            // `aube_config` outranks `~/.npmrc`, so emit it last — the
            // reversed-iteration lookup below returns the first match,
            // i.e. the highest-precedence source for the requested key.
            let mut entries = read_single(&user_npmrc_path()?)?;
            entries.extend(super::aube_config::load_user_entries());
            entries
        }
        ListLocation::Project => {
            // Project-scope precedence (low → high): workspace yaml,
            // project `.npmrc`, project `config.toml`.
            let mut entries = super::read_workspace_yaml_flat(&cwd);
            entries.extend(read_single(&cwd.join(".npmrc"))?);
            entries.extend(super::aube_config::load_project_entries(&cwd));
            entries
        }
    };

    let managed_entries = super::aube_config::load_managed_entries();
    if matches!(args.effective_location(), ListLocation::Merged)
        && !managed_entries.is_empty()
        && let Some(meta) = setting_for_key(&args.key)
    {
        let local = entries
            .iter()
            .rev()
            .find_map(|(k, v)| aliases.iter().any(|a| a == k).then(|| v.clone()));
        if let Some(v) = aube_settings::values::apply_managed_raw(
            meta.name,
            local.or_else(|| setting_default_value(meta)),
            &managed_entries,
        ) {
            if args.json {
                println!("{}", serde_json::Value::String(v));
            } else {
                println!("{v}");
            }
            return Ok(());
        }
    }

    for (k, v) in entries.iter().rev() {
        if aliases.iter().any(|a| a == k) {
            if args.json {
                println!("{}", serde_json::Value::String(v.clone()));
            } else {
                println!("{v}");
            }
            return Ok(());
        }
    }
    println!("undefined");
    Ok(())
}
