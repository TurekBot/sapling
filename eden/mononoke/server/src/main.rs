/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]
#![feature(never_type)]

use anyhow::{Context, Result};
use cloned::cloned;
use cmdlib::{args, monitoring::ReadyFlagService};
use fbinit::FacebookInit;
use futures::channel::oneshot;
use mononoke_api::{
    BookmarkUpdateDelay, Mononoke, MononokeEnvironment, WarmBookmarksCacheDerivedData,
};
use openssl::ssl::AlpnError;
use slog::{error, info};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn setup_app<'a, 'b>() -> args::MononokeClapApp<'a, 'b> {
    let app = args::MononokeAppBuilder::new("mononoke server")
        .with_shutdown_timeout_args()
        .with_all_repos()
        .with_disabled_hooks_args()
        .build()
        .about("serve repos")
        .args_from_usage(
            r#"
            --listening-host-port <PATH>           'tcp address to listen to in format `host:port`'

            -p, --thrift_port [PORT] 'if provided the thrift server will start on this port'

            <cert>        --cert [PATH]                         'path to a file with certificate'
            <private_key> --private-key [PATH]                  'path to a file with private key'
            <ca_pem>      --ca-pem [PATH]                       'path to a file with CA certificate'
            [ticket_seed] --ssl-ticket-seeds [PATH]             'path to a file with encryption keys for SSL tickets'
            "#,
        );

    let app = args::add_mcrouter_args(app);
    let app = args::add_scribe_logging_args(app);
    app
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let matches = setup_app().get_matches();
    cmdlib::args::maybe_enable_mcrouter(fb, &matches);

    let (caching, root_log, runtime) = cmdlib::args::init_mononoke(fb, &matches)?;
    let config_store = cmdlib::args::init_config_store(fb, &root_log, &matches)?;
    let observability_context = cmdlib::args::init_observability_context(fb, &matches, &root_log)?;

    info!(root_log, "Starting up");

    let config = args::load_repo_configs(config_store, &matches)?;

    let acceptor = {
        let cert = matches.value_of("cert").unwrap().to_string();
        let private_key = matches.value_of("private_key").unwrap().to_string();
        let ca_pem = matches.value_of("ca_pem").unwrap().to_string();

        let mut builder = secure_utils::SslConfig::new(
            ca_pem,
            cert,
            private_key,
            matches.value_of("ssl-ticket-seeds"),
        )
        .tls_acceptor_builder(root_log.clone())
        .context("Failed to instantiate TLS Acceptor builder")?;

        builder.set_alpn_select_callback(|_, protos| {
            // NOTE: Currently we do not support HTTP/2 here yet.
            alpn::alpn_select(protos, alpn::HGCLI_ALPN)
                .map_err(|_| AlpnError::ALERT_FATAL)?
                .ok_or(AlpnError::NOACK)
        });

        builder.build()
    };

    info!(root_log, "Creating repo listeners");

    let service = ReadyFlagService::new();
    let (terminate_sender, terminate_receiver) = oneshot::channel::<()>();

    let mysql_options = cmdlib::args::parse_mysql_options(&matches);
    let disabled_hooks = cmdlib::args::parse_disabled_hooks_with_repo_prefix(&matches, &root_log)?;
    let scribe = cmdlib::args::get_scribe(fb, &matches)?;
    let is_test = cmdlib::args::is_test_instance(&matches);
    let host_port = matches
        .value_of("listening-host-port")
        .expect("listening path must be specified")
        .to_string();
    let readonly_storage = cmdlib::args::parse_readonly_storage(&matches);
    let blobstore_options = cmdlib::args::parse_blobstore_options(&matches)?;

    let will_exit = Arc::new(AtomicBool::new(false));

    let repo_listeners = {
        cloned!(root_log, service, will_exit);
        async move {
            let env = MononokeEnvironment {
                fb,
                logger: root_log.clone(),
                mysql_options: mysql_options.clone(),
                caching,
                readonly_storage,
                blobstore_options,
                config_store,
                disabled_hooks,
                warm_bookmarks_cache_derived_data: WarmBookmarksCacheDerivedData::HgOnly,
                warm_bookmarks_cache_delay: BookmarkUpdateDelay::Disallow,
            };

            let mononoke = Mononoke::new(&env, config.clone()).await?;

            repo_listener::create_repo_listeners(
                fb,
                is_test,
                config.common,
                mononoke,
                &mysql_options,
                root_log,
                host_port,
                acceptor,
                service,
                terminate_receiver,
                config_store,
                readonly_storage,
                scribe,
                &observability_context,
                will_exit,
            )
            .await
        }
    };

    #[cfg(fbcode_build)]
    {
        tracing_fb303::register(fb);
    }

    // Thread with a thrift service is now detached
    monitoring::start_thrift_service(fb, &root_log, &matches, service);

    cmdlib::helpers::serve_forever(
        runtime,
        repo_listeners,
        &root_log,
        move || will_exit.store(true, Ordering::Relaxed),
        args::get_shutdown_grace_period(&matches)?,
        async {
            match terminate_sender.send(()) {
                Err(err) => error!(root_log, "could not send termination signal: {:?}", err),
                _ => {}
            }
            repo_listener::wait_for_connections_closed().await;
        },
        args::get_shutdown_timeout(&matches)?,
    )
}
