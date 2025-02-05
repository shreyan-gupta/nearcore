use anyhow::Context;
use std::cell::Cell;
use std::path::PathBuf;

use near_primitives::types::BlockHeight;
use near_primitives::views::AccessKeyPermissionView;

#[derive(clap::Parser)]
pub struct MirrorCommand {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(clap::Parser)]
enum SubCommand {
    Prepare(PrepareCmd),
    Run(RunCmd),
    ShowKeys(ShowKeysCmd),
}

/// initialize a target chain with genesis records from the source chain, and
/// then try to mirror transactions from the source chain to the target chain.
#[derive(clap::Parser)]
struct RunCmd {
    /// source chain home dir
    #[clap(long)]
    source_home: PathBuf,
    /// target chain home dir
    #[clap(long)]
    target_home: PathBuf,
    /// mirror database dir
    #[clap(long)]
    mirror_db_path: Option<PathBuf>,
    /// file containing an optional secret as generated by the
    /// `prepare` command. Must be provided unless --no-secret is given
    #[clap(long)]
    secret_file: Option<PathBuf>,
    /// Equivalent to passing --secret-file <FILE> where <FILE> is a
    /// config that indicates no secret should be used. If this is
    /// given, and --secret-file is also given and points to a config
    /// that does contain a secret, the mirror will refuse to start
    #[clap(long)]
    no_secret: bool,
    /// Start a NEAR node for the source chain, instead of only using
    /// whatever's currently stored in --source-home
    #[clap(long)]
    online_source: bool,
    /// If provided, we will stop after sending transactions coming from
    /// this height in the source chain
    #[clap(long)]
    stop_height: Option<BlockHeight>,
    #[clap(long)]
    config_path: Option<PathBuf>,
}

impl RunCmd {
    fn run(self) -> anyhow::Result<()> {
        openssl_probe::init_ssl_cert_env_vars();

        let secret = if let Some(secret_file) = &self.secret_file {
            let secret = crate::secret::load(secret_file)
                .with_context(|| format!("Failed to load secret from {:?}", secret_file))?;
            if secret.is_some() && self.no_secret {
                anyhow::bail!(
                    "--no-secret given with --secret-file indicating that a secret should be used"
                );
            }
            secret
        } else {
            if !self.no_secret {
                anyhow::bail!("Please give either --secret-file or --no-secret");
            }
            None
        };

        run_async(crate::run(
            self.source_home,
            self.target_home,
            self.mirror_db_path,
            secret,
            self.stop_height,
            self.online_source,
            self.config_path,
        ))
    }
}

/// Write a new genesis records file where the public keys have been
/// altered so that this binary can sign transactions when mirroring
/// them from the source chain to the target chain
#[derive(clap::Parser)]
struct PrepareCmd {
    /// A genesis records file as output by `neard view-state
    /// dump-state --stream`
    #[clap(long)]
    records_file_in: PathBuf,
    /// Path to the new records file with updated public keys
    #[clap(long)]
    records_file_out: PathBuf,
    /// If this is provided, don't use a secret when mapping public
    /// keys to new source chain private keys. This means that anyone
    /// will be able to sign transactions for the accounts in the
    /// target chain corresponding to accounts in the source chain. If
    /// that is okay, then --no-secret will make the code run slightly
    /// faster, and you won't have to take care to not lose the
    /// secret.
    #[clap(long)]
    no_secret: bool,
    /// Path to the secret. Note that if you don't pass --no-secret,
    /// this secret is required to sign transactions for the accounts
    /// in the target chain corresponding to accounts in the source
    /// chain. This means that if you lose this secret, you will no
    /// longer be able to mirror any traffic.
    #[clap(long)]
    secret_file_out: PathBuf,
}

impl PrepareCmd {
    fn run(self) -> anyhow::Result<()> {
        crate::genesis::map_records(
            &self.records_file_in,
            &self.records_file_out,
            self.no_secret,
            &self.secret_file_out,
        )
    }
}

/// Given a source chain NEAR home dir, read and map access keys corresponding to
/// a given account ID and optional block height.
#[derive(clap::Parser)]
struct ShowKeysFromSourceDBCmd {
    #[clap(long)]
    home: PathBuf,
    #[clap(long)]
    account_id: String,
    #[clap(long)]
    block_height: Option<BlockHeight>,
}

/// Given an RPC URL for a node running on the source chain (so for a network forked from mainnet state,
/// a mainnet RPC node), request and map access keys corresponding to a given account ID and optional block height.
#[derive(clap::Parser)]
struct ShowKeysFromRPCCmd {
    /// RPC URL for a node running on the source chain. e.g. "https://rpc.mainnet.near.org"
    #[clap(long)]
    rpc_url: String,
    #[clap(long)]
    account_id: String,
    #[clap(long)]
    block_height: Option<BlockHeight>,
}

/// Map the given public key
#[derive(clap::Parser)]
struct ShowKeyFromKeyCmd {
    #[clap(long)]
    public_key: String,
}

/// Show the default extra key. This key should exist for any account that does not have
/// any full access keys in the source chain (e.g. validators with staking pools)
#[derive(clap::Parser)]
struct ShowDefaultExtraKeyCmd;

#[derive(clap::Parser)]
enum ShowKeysSubCommand {
    FromSourceDB(ShowKeysFromSourceDBCmd),
    FromRPC(ShowKeysFromRPCCmd),
    FromPubKey(ShowKeyFromKeyCmd),
    DefaultExtraKey(ShowDefaultExtraKeyCmd),
}

/// Print the secret keys that correspond to source chain public keys
#[derive(clap::Parser)]
struct ShowKeysCmd {
    /// file containing an optional secret as generated by the
    /// `prepare` command.
    #[clap(long)]
    secret_file: Option<PathBuf>,
    #[clap(subcommand)]
    subcmd: ShowKeysSubCommand,
}

impl ShowKeysCmd {
    fn run(self) -> anyhow::Result<()> {
        let secret = if let Some(secret_file) = &self.secret_file {
            let secret = crate::secret::load(secret_file)
                .with_context(|| format!("Failed to load secret from {:?}", secret_file))?;
            secret
        } else {
            None
        };
        let mut probably_extra_key = false;
        let keys = match self.subcmd {
            ShowKeysSubCommand::FromSourceDB(c) => {
                let keys = crate::key_util::keys_from_source_db(
                    &c.home,
                    &c.account_id,
                    c.block_height,
                    secret.as_ref(),
                )?;
                probably_extra_key = keys.iter().all(|key| {
                    key.permission
                        .as_ref()
                        .map_or(true, |p| *p != AccessKeyPermissionView::FullAccess)
                });
                keys
            }
            ShowKeysSubCommand::FromRPC(c) => {
                let keys = run_async(async move {
                    crate::key_util::keys_from_rpc(
                        &c.rpc_url,
                        &c.account_id,
                        c.block_height,
                        secret.as_ref(),
                    )
                    .await
                })?;
                probably_extra_key = keys.iter().all(|key| {
                    key.permission
                        .as_ref()
                        .map_or(true, |p| *p != AccessKeyPermissionView::FullAccess)
                });
                keys
            }
            ShowKeysSubCommand::FromPubKey(c) => {
                vec![crate::key_util::map_pub_key(&c.public_key, secret.as_ref())?]
            }
            ShowKeysSubCommand::DefaultExtraKey(_c) => {
                vec![crate::key_util::default_extra_key(secret.as_ref())]
            }
        };
        for key in keys.iter() {
            if let Some(k) = &key.original_key {
                println!("original pub key: {}", k);
            }
            println!(
                "mapped secret key: {}\nmapped public key: {}",
                &key.mapped_key,
                key.mapped_key.public_key()
            );
            if let Some(a) = &key.permission {
                println!("access: {:?}", a);
            }
            println!("------------")
        }
        if probably_extra_key {
            let extra_key = crate::key_mapping::default_extra_key(secret.as_ref());
            println!(
                "{} account probably has an extra full access key added:\nmapped secret key: {}\npublic key: {}",
                if keys.is_empty() { "If it exists, this" } else { "This" },
                &extra_key, extra_key.public_key(),
            );
        }
        Ok(())
    }
}

// copied from neard/src/cli.rs
fn new_actix_system(runtime: tokio::runtime::Runtime) -> actix::SystemRunner {
    // `with_tokio_rt()` accepts an `Fn()->Runtime`, however we know that this function is called exactly once.
    // This makes it safe to move out of the captured variable `runtime`, which is done by a trick
    // using a `swap` of `Cell<Option<Runtime>>`s.
    let runtime_cell = Cell::new(Some(runtime));
    actix::System::with_tokio_rt(|| {
        let r = Cell::new(None);
        runtime_cell.swap(&r);
        r.into_inner().unwrap()
    })
}

fn run_async<F: std::future::Future + 'static>(f: F) -> F::Output {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let system = new_actix_system(runtime);
    system
        .block_on(async move {
            let _subscriber_guard = near_o11y::default_subscriber(
                near_o11y::EnvFilterBuilder::from_env().finish().unwrap(),
                &near_o11y::Options::default(),
            )
            .global();
            actix::spawn(f).await
        })
        .unwrap()
}

impl MirrorCommand {
    pub fn run(self) -> anyhow::Result<()> {
        tracing::warn!(target: "mirror", "the mirror command is not stable, and may be removed or changed arbitrarily at any time");

        match self.subcmd {
            SubCommand::Prepare(r) => r.run(),
            SubCommand::Run(r) => r.run(),
            SubCommand::ShowKeys(r) => r.run(),
        }
    }
}
