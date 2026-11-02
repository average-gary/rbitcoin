use rbitcoin_consensus::default_milestone_height;
use rbitcoin_node::cli::*;
use rbitcoin_node::config::NodeEnv;
use rbitcoin_node::config::{ConfApply, NodeConfig};
use rbitcoin_primitives::Network;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
/// [`NodeEnv::from_lookup`] over literal pairs, matched exactly. The last pair
/// for a name wins.
fn env_of(vars: &[(&str, &str)]) -> Result<NodeEnv, rbitcoin_node::NodeError> {
    NodeEnv::from_lookup(|k| {
        vars.iter()
            .rev()
            .find(|(name, _)| *name == k)
            .map(|(_, v)| OsString::from(v))
    })
}

fn tmp_datadir() -> PathBuf {
    rbitcoin_store::testutil::fresh_path("node-cli")
}

/// `ExitCode` is not `PartialEq`; compare via `Debug` (stable, sufficient for tests).
fn assert_exit(got: ExitCode, want: ExitCode) {
    assert_eq!(
        format!("{got:?}"),
        format!("{want:?}"),
        "exit code mismatch"
    );
}

#[test]
fn blocking_pool_is_capped_not_tokio_default() {
    assert!(blocking_pool_size() >= 4);
    assert!(blocking_pool_size() <= 512);
}

#[test]
fn proposal_validations_use_a_quarter_of_the_blocking_pool() {
    let pool = blocking_pool_size();
    let permits = proposal_validation_permits();
    assert!(permits >= 1, "a four-thread node runs one at a time");
    assert!(
        permits * 4 <= pool,
        "at most a quarter of {pool}: {permits}"
    );
    assert!(
        (permits + 1) * 4 > pool,
        "the largest such count: {permits}"
    );
}

fn ready_config<I, T>(args: I) -> NodeConfig
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    match operator_config_from_args(args, &NodeEnv::default()) {
        Ok(OperatorArgs::Ready { config, .. }) => config,
        other => panic!("expected assembled config, got {other:?}"),
    }
}

#[test]
fn cli_onion_bind_replaces_file_listeners() {
    let dir = rbitcoin_store::testutil::TempDir::new().unwrap();
    let conf = dir.path().join("rbitcoin.conf");
    std::fs::write(
        &conf,
        "listen=127.0.0.1:18333\nlisten=127.0.0.1:18334=onion\n",
    )
    .unwrap();
    let cfg = ready_config([
        "rbitcoin-node",
        "--conf",
        conf.to_str().unwrap(),
        "--listen=[::1]:28334=onion",
    ]);
    assert_eq!(
        cfg.listen.start_p2p_bind(cfg.network),
        Some("[::1]:28334".parse().unwrap())
    );
    assert!(cfg.listen.p2p_bind_addr(cfg.network).is_none());
    assert!(cfg.listen.p2p_extra.is_empty());
}

#[test]
fn external_ip_cli_replaces_only_matching_file_hosts() {
    let dir = rbitcoin_store::testutil::TempDir::new().unwrap();
    let conf = dir.path().join("rbitcoin.conf");
    std::fs::write(
        &conf,
        "external_ip=8.8.8.8:18333\n\
         external_ip=42.42.42.42\n\
         external_ip=8.8.8.8:18334\n\
         external_ip=[::ffff:42.42.42.42]:18335\n",
    )
    .unwrap();
    let cfg = ready_config([
        "rbitcoin-node",
        "--conf",
        conf.to_str().unwrap(),
        "--external-ip=9.9.9.9:28333",
        "--external-ip=[::ffff:42.42.42.42]:28334",
        "--external-ip=42.42.42.42:28335",
    ]);
    assert_eq!(
        cfg.listen.external_endpoints,
        [
            "8.8.8.8:18333",
            "42.42.42.42:28334",
            "42.42.42.42:28335",
            "8.8.8.8:18334",
            "9.9.9.9:28333",
        ]
        .map(|endpoint| endpoint.parse().unwrap())
    );
}

/// What argv and the conf file assemble before `run_node`. The smoke
/// journey (`node_cli_and_surface_smoke`) sees only the exit code and
/// the datadir; these values and the help text are not reported by any
/// running surface, so this is the one place they are read.
#[allow(clippy::cognitive_complexity)] // one operator's conf and argv
#[test]
fn operator_conf_and_argv() {
    use rbitcoin_consensus::Milestone;
    use rbitcoin_node::config::P2pListen;
    let h = operator_usage();
    for flag in [
        "--prefill-compact",
        "--limit-cluster-count",
        "--limit-cluster-size",
        "--min-relay-tx-fee",
        "--mempool-expiry",
        "--external-ip",
        "--seed-node",
        "--mock-time",
        "--block-version",
        "--block-min-tx-fee",
        "--bytes-per-sigop",
        "--block-reserved-sigops",
        "--alert-notify",
        "--startup-notify",
        "--test-activation-height",
        "--regtest-pow-bits",
        "--regtest-block-time",
        "--regtest-retarget-interval",
        "--rpc-work-queue",
        "--rpc-client-version",
        "--peer-timeout",
        "--blocks-only",
        "--ua-comment",
        "--min-chain-work",
        "--max-tip-age",
        "--check-blocks",
        "--net-permission",
        "--net-permission-bind",
        "--net-permission-relay",
        "--net-permission-force-relay",
        "--signet-block-time",
        "--sh-index",
        "--block-filter-index",
        "--prune-seqsigwit",
        "--datadir-hdd",
        "--prune-seqsigwit-ram-threshold-bytes",
        "--sp-tweaks",
        "--sp-tweaks-dust",
        "--esplora-block-template",
        "--electrum-onion",
        "--esplora-onion",
        "--electrum-i2p",
        "--esplora-i2p",
        "--sv2-tp-listen",
        "--sv2-tp-authority-sec",
        "--sv2-tp-authority-sec-file",
        "--sv2-tp-cert-validity",
        "--sv2-tp-stale-grace",
        "--sv2-tp-idle-timeout",
        "--sv2-tp-fee-delta",
        "--sv2-tp-template-interval",
        "--rpc",
        "--rpc-listen",
        "--rest",
        "--rpc-socket",
        "--rpc-token-file",
        "--rpc-cookie-file",
        "--rpc-cookie-perms",
        "--proxy",
        "--onion",
        "--proxy-randomize",
        "--no-listen",
        "--no-discover",
        "--discover-from-peers",
        "--listen-onion",
        "--cjdns-reachable",
        "--tor-control",
        "--tor-control-cookie",
        "--tor-control-password",
        "--i2p-sam",
        "--i2p-accept-incoming",
    ] {
        assert!(h.contains(flag), "help must list {flag}");
    }
    for concat in [
        "--shindex",
        "--sptweaks",
        "-shindex",
        "-sptweaks",
        "--pruneseqsigwit",
        "--prefillcompact",
        "--limitclustercount",
        "--limitclustersize",
        "--minrelaytxfee",
        "--mempoolexpiry",
        "--externalip",
        "--seednode",
        "--mocktime",
        "--blockversion",
        "--blockmintxfee",
        "--alertnotify",
        "--startupnotify",
        "--testactivationheight",
        "--regtestpowbits",
        "--regtestblocktime",
        "--regtestretargetinterval",
        "--rpcworkqueue",
        "--checkblocks",
        "--blocksdir",
        "--blocks-dir",
        "--whitelist-relay",
        "--whitelist-forcerelay",
        "--nolisten",
        "--nodiscover",
        "--listenonion",
        "--cjdnsreachable",
        "--torcontrol",
        "--i2psam",
    ] {
        assert!(!h.contains(concat), "help must not advertise {concat}");
    }
    assert!(h.contains("Networks: mainnet|testnet|testnet4|testnet5|signet|regtest."));
    assert!(h.contains("[--signet-block-time SECS]"));
    assert!(matches!(
        operator_config_from_args(["rbitcoin-node", "-V"], &NodeEnv::default()),
        Ok(OperatorArgs::Version)
    ));
    assert!(matches!(
        operator_config_from_args(["rbitcoin-node", "--log-level=off"], &NodeEnv::default()),
        Ok(OperatorArgs::Ready {
            log_level_cli: Some(None),
            ..
        })
    ));

    let omitted = ready_config(["rbitcoin-node"]);
    assert_eq!(omitted.network, Network::Mainnet);
    assert_eq!(
        omitted.milestone_height,
        default_milestone_height(Network::Mainnet)
    );
    assert!(omitted.milestone().anchor.is_some());
    assert!(!omitted.milestone().skips_scripts_at(1));
    assert!(!omitted.meets_minimum_chain_work([0; 32]));
    assert!(omitted.prefill_compact);
    assert_eq!(omitted.check_blocks, None);
    assert_eq!(
        omitted.check_blocks_window(),
        rbitcoin_store::VERIFY_TIP_BLOCKS
    );
    assert!(omitted.listen.discover);
    assert_eq!(omitted.listen.p2p, P2pListen::Auto);
    assert_eq!(
        omitted.listen.p2p_bind_addr(Network::Regtest),
        Some("127.0.0.1:18444".parse().unwrap())
    );

    let dir = tmp_datadir();
    std::fs::create_dir_all(&dir).unwrap();
    let knobs = dir.join("rbitcoin.conf");
    std::fs::write(
        &knobs,
        "# custom signet\n\
         network=signet\n\
         signet_challenge=51\n\
         signet_block_time=60\n\
         max_inbound=40\n\
         max_outbound=8\n\
         mempool_size_mb=50\n\
         milestone=100\n\
         log_level=debug\n\
         api_log=/tmp/rbitcoin-api.jsonl\n\
         asmap=/tmp/ip_asn.dat\n\
         connect=127.0.0.1:38333\n\
         datadir-cold=/mnt/hdd/rbtc-cold\n",
    )
    .unwrap();
    let cfg = ready_config(["rbitcoin-node", "--conf", knobs.to_str().unwrap()]);
    assert_eq!(cfg.network, Network::Signet);
    let params = cfg.chain_params().unwrap();
    assert_eq!(params.btc.pow_target_spacing, 60);
    assert_eq!(params.signet_challenge.unwrap().as_bytes(), &[0x51]);
    assert_eq!(cfg.listen.max_inbound, 40);
    assert!(cfg.listen.max_inbound_explicit);
    assert_eq!(cfg.listen.max_outbound, 8);
    assert_eq!(cfg.mempool.max_weight, 50_000_000);
    assert_eq!(cfg.milestone_height, 100);
    assert_eq!(cfg.conf_log_level.as_deref(), Some("debug"));
    assert_eq!(
        cfg.api_log.as_deref(),
        Some(std::path::Path::new("/tmp/rbitcoin-api.jsonl"))
    );
    assert_eq!(
        cfg.asmap.as_deref(),
        Some(std::path::Path::new("/tmp/ip_asn.dat"))
    );
    assert_eq!(cfg.listen.connect.len(), 1);
    assert!(cfg.listen.connect_dns.is_empty());
    assert_eq!(
        cfg.datadir.cold.as_deref(),
        Some(std::path::Path::new("/mnt/hdd/rbtc-cold"))
    );

    // Bare network lines, and an explicit milestone 0 that the network
    // default does not replace, from the conf and from argv alike.
    for (bare, net) in [
        ("regtest", Network::Regtest),
        ("signet", Network::Signet),
        ("testnet", Network::Testnet),
        ("testnet4", Network::Testnet4),
        ("testnet5", Network::Testnet5),
    ] {
        let conf = dir.join(format!("{bare}.conf"));
        std::fs::write(&conf, format!("{bare}\n; also a comment\n\nno_seeds=1\n")).unwrap();
        let eq = format!("--conf={}", conf.to_str().unwrap());
        let cfg = ready_config(["rbitcoin-node", eq.as_str()]);
        assert_eq!(cfg.network, net);
        assert!(!cfg.listen.use_seeds);
        let mut keyed = NodeConfig::default();
        assert_eq!(keyed.apply_kv(bare, "1").unwrap(), ConfApply::Applied);
        assert_eq!(keyed.network, net);
    }
    let zero = dir.join("milestone.conf");
    std::fs::write(&zero, "milestone=0\n").unwrap();
    for cfg in [
        ready_config(["rbitcoin-node", "--conf", zero.to_str().unwrap()]),
        ready_config(["rbitcoin-node", "--milestone", "0"]),
    ] {
        assert_eq!(cfg.network, Network::Mainnet);
        assert_eq!(cfg.milestone(), Milestone::NONE);
    }
    // Conf `sp_tweaks` and `sp_tweaks_dust`, including dust 0 (serve every
    // output) and a value that does not parse.
    let sp = dir.join("sp.conf");
    std::fs::write(&sp, "sp_tweaks=1\n").unwrap();
    let sp_cfg = ready_config(["rbitcoin-node", "--conf", sp.to_str().unwrap()]);
    assert!(sp_cfg.sptweaks);
    assert_eq!(
        sp_cfg.sptweaks_dust,
        rbitcoin_electrum::DEFAULT_TWEAKS_MIN_DUST
    );
    sp_cfg.validate().expect("conf sp_tweaks=1");
    let dust = dir.join("dust.conf");
    std::fs::write(&dust, "sp_tweaks_dust=546\n").unwrap();
    assert_eq!(
        ready_config(["rbitcoin-node", "--conf", dust.to_str().unwrap()]).sptweaks_dust,
        546
    );
    let dust0 = dir.join("dust0.conf");
    std::fs::write(&dust0, "sp_tweaks_dust=0\n").unwrap();
    assert_eq!(
        ready_config(["rbitcoin-node", "--conf", dust0.to_str().unwrap()]).sptweaks_dust,
        0
    );
    let bad_dust = dir.join("dust-bad.conf");
    std::fs::write(&bad_dust, "sp_tweaks_dust=nope\n").unwrap();
    let err = NodeConfig::default()
        .merge_conf_file(&bad_dust)
        .unwrap_err()
        .to_string();
    assert!(err.contains("sp_tweaks_dust"), "{err}");
    match operator_config_from_args(
        ["rbitcoin-node", "--conf", bad_dust.to_str().unwrap()],
        &NodeEnv::default(),
    ) {
        Err(code) => assert_exit(code, ExitCode::from(2)),
        Ok(other) => panic!("bad sp_tweaks_dust conf assembled: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    let explicit = ready_config(["rbitcoin-node", "--milestone", "840000"]);
    assert!(explicit.milestone().anchor.is_none());
    assert!(explicit.milestone().skips_scripts_at(1));
    let signet = ready_config(["rbitcoin-node", "--network=signet"]);
    assert_eq!(signet.milestone_height, 0);
    assert!(!signet.milestone().skips_scripts_at(1));
    let testnet = ready_config(["rbitcoin-node", "--network=testnet"]);
    assert_eq!(testnet.milestone_height, 5_128_859);
    assert!(testnet.milestone().anchor.is_some());
    assert!(
        !testnet.milestone().skips_scripts_at(1),
        "anchored testnet milestone does not skip on height alone"
    );
    let testnet4 = ready_config(["rbitcoin-node", "--network=testnet4"]);
    assert_eq!(testnet4.milestone_height, 0);
    assert!(testnet4.milestone().anchor.is_none());
    assert!(!testnet4.milestone().skips_scripts_at(1));
    assert_eq!(
        testnet4.minimum_chain_work,
        rbitcoin_consensus::default_min_chain_work_be(Network::Testnet4)
    );
    assert!(testnet4.minimum_chain_work.is_some());
    let testnet5 = ready_config(["rbitcoin-node", "--network=testnet5"]);
    assert_eq!(testnet5.network, Network::Testnet5);
    assert_eq!(testnet5.milestone_height, 0);
    assert!(testnet5.milestone().anchor.is_none());
    assert!(testnet5.minimum_chain_work.is_none());
    assert_eq!(
        testnet5.chain_params().unwrap().genesis_hash,
        rbitcoin_consensus::ChainParams::testnet5().genesis_hash
    );
    let signet_skip = ready_config([
        "rbitcoin-node",
        "--network=signet",
        "--milestone",
        "2000000",
    ]);
    assert!(signet_skip.milestone().skips_scripts_at(1));
    let work = ready_config(["rbitcoin-node", "--min-chain-work=0x65"]);
    assert_eq!(work.minimum_chain_work.unwrap()[31], 0x65);
    let mut long_work = NodeConfig::default();
    let long_hex = "01234567890123456789012345678901234567890123456789012345678901234";
    let refused = long_work
        .apply_kv("min_chain_work", long_hex)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("64 hex digits"),
        "65 hex digits must be refused: {refused}"
    );

    // Kebab flags set the knob the conf key of the same name sets.
    let cfg = ready_config([
        "rbitcoin-node",
        "--network",
        "regtest",
        "--sh-index",
        "--sp-tweaks",
        "--sp-tweaks-dust=546",
        "--max-inbound",
        "0",
        "--no-discover",
        "--seed-node",
        "127.0.0.1:8333",
        "--min-relay-tx-fee",
        "0.00001000",
        "--rpc-listen",
        "--electrum-listen",
        "--esplora-listen",
        "--health-listen",
        "--metrics",
    ]);
    assert!(cfg.shindex && cfg.sptweaks);
    assert_eq!(cfg.sptweaks_dust, 546);
    assert_eq!(cfg.listen.max_inbound, 0);
    assert!(cfg.listen.max_inbound_explicit);
    assert!(!cfg.listen.discover);
    assert_eq!(cfg.listen.seednodes, vec!["127.0.0.1:8333".to_string()]);
    assert_eq!(cfg.mempool.min_relay_fee_btc.as_deref(), Some("0.00001000"));
    assert!(cfg.rpc.socket);
    assert_eq!(cfg.rpc.listen, Some("127.0.0.1:18443".parse().unwrap()));
    assert_eq!(cfg.listen.electrum.unwrap().port(), 50001);
    assert_eq!(cfg.listen.health, Some("127.0.0.1:9332".parse().unwrap()));
    assert!(cfg.metrics);
    match cfg.listen.esplora.unwrap() {
        rbitcoin_esplora::EsploraListen::Tcp(a) => assert_eq!(a.port(), 3000),
        #[cfg(unix)]
        rbitcoin_esplora::EsploraListen::Unix(_) => panic!("default esplora-listen is TCP"),
    }
    #[cfg(unix)]
    match ready_config(["rbitcoin-node", "--esplora-listen", "/tmp/esplora.sock"])
        .listen
        .esplora
        .unwrap()
    {
        rbitcoin_esplora::EsploraListen::Unix(p) => {
            assert_eq!(p, PathBuf::from("/tmp/esplora.sock"))
        }
        rbitcoin_esplora::EsploraListen::Tcp(_) => panic!("path must be unix"),
    }
    let sock = ready_config(["rbitcoin-node", "--rpc"]);
    assert!(sock.rpc.socket && sock.rpc.listen.is_none());
    let pruned = ready_config([
        "rbitcoin-node",
        "--prune-seqsigwit",
        "--prune-seqsigwit-ram-threshold-bytes=4096",
    ]);
    assert!(pruned.prune_seqsigwit);
    assert_eq!(pruned.prune_seqsigwit_ram_threshold_bytes, 4096);
    assert!(!ready_config(["rbitcoin-node"]).datadir_hdd);
    assert!(ready_config(["rbitcoin-node", "--datadir-hdd"]).datadir_hdd);
    let mut hdd_conf = NodeConfig::default();
    hdd_conf.apply_kv("datadir_hdd", "1").unwrap();
    assert!(hdd_conf.datadir_hdd);
    for (argv, want) in [
        (&["--prefill-compact=0"][..], false),
        (&["--prefill-compact"], true),
        (&["--prefill-compact=1"], true),
    ] {
        let mut args = vec!["rbitcoin-node"];
        args.extend_from_slice(argv);
        assert_eq!(ready_config(args).prefill_compact, want, "{argv:?}");
    }
    for (arg, stored, window) in [
        ("--check-blocks=6", 6, 6),
        ("--check-blocks=0", 0, 0),
        ("--check-blocks=-1", -1, 0),
    ] {
        let cfg = ready_config(["rbitcoin-node", arg]);
        assert_eq!(cfg.check_blocks, Some(stored));
        assert_eq!(cfg.check_blocks_window(), window, "{arg}");
    }
    for off in ["--no-listen", "--listen=0"] {
        let cfg = ready_config(["rbitcoin-node", off]);
        assert_eq!(cfg.listen.p2p, P2pListen::Off, "{off}");
        assert!(cfg.listen.p2p_bind_addr(Network::Regtest).is_none());
    }
    let bound = ready_config(["rbitcoin-node", "--listen", "127.0.0.1:18445"]);
    assert_eq!(
        bound.listen.p2p_bind_addr(Network::Regtest),
        Some("127.0.0.1:18445".parse().unwrap())
    );

    assert_eq!(
        ready_config(["rbitcoin-node"]).mempool.bytes_per_sigop,
        None
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--bytes-per-sigop", "0"])
            .mempool
            .bytes_per_sigop,
        Some(0)
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--bytes-per-sigop=40"])
            .mempool
            .bytes_per_sigop,
        Some(40)
    );
    assert_exit(
        cli_main(["rbitcoin-node", "--bytes-per-sigop=x"], NodeEnv::default()),
        ExitCode::from(2),
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--block-reserved-sigops", "0"])
            .mempool
            .block_reserved_sigops,
        Some(0)
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--block-reserved-sigops=400"])
            .mempool
            .block_reserved_sigops,
        Some(400)
    );
    let mut over = ready_config(["rbitcoin-node", "--block-reserved-sigops", "80001"]);
    assert!(over.validate().is_err());
    over.mempool.block_reserved_sigops = Some(80_000);
    over.validate().expect("consensus maximum is accepted");

    ready_config(["rbitcoin-node", "--sp-tweaks", "--prune-seqsigwit"])
        .validate()
        .expect("prune and tweaks validate together");
    let mut conf = NodeConfig::default();
    conf.apply_kv("prune_seqsigwit", "1").unwrap();
    conf.apply_kv("sp_tweaks", "1").unwrap();
    conf.validate()
        .expect("conf prune_seqsigwit with sp_tweaks validates");

    // Electrum and Esplora do not require the scripthash index. The index
    // may also stand alone, or run with either listener.
    for argv in [
        &["--electrum-listen"][..],
        &["--esplora-listen"],
        &["--sh-index"],
        &["--sh-index", "--electrum-listen"],
        &["--sh-index", "--esplora-listen"],
    ] {
        let mut args = vec!["rbitcoin-node", "--network", "regtest"];
        args.extend_from_slice(argv);
        ready_config(args)
            .validate()
            .unwrap_or_else(|e| panic!("{argv:?} must validate: {e}"));
    }

    // Zero is the finite default queue, not an unbounded one.
    assert_eq!(
        ready_config(["rbitcoin-node"]).rpc.work_queue,
        Some(rbitcoin_rpc::DEFAULT_RPC_WORK_QUEUE)
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--rpc-work-queue=0"])
            .rpc
            .work_queue,
        Some(rbitcoin_rpc::DEFAULT_RPC_WORK_QUEUE)
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--rpc-work-queue=4"])
            .rpc
            .work_queue,
        Some(4)
    );

    // Only the reported integer; 190000 unless an operator raises it.
    assert_eq!(ready_config(["rbitcoin-node"]).rpc.client_version, 190_000);
    assert_eq!(
        ready_config(["rbitcoin-node", "--rpc-client-version=310000"])
            .rpc
            .client_version,
        310_000
    );
    let mut refused = NodeConfig::default();
    let zero = refused.apply_kv("rpc_client_version", "0").unwrap_err();
    assert!(zero.to_string().contains("between 1 and 1000000"), "{zero}");
    assert_eq!(refused.rpc.client_version, 190_000);
    for bad in ["--rpc-client-version=0", "--rpc-client-version=1000001"] {
        match operator_config_from_args(["rbitcoin-node", bad], &NodeEnv::default()) {
            Err(code) => assert_exit(code, ExitCode::from(2)),
            Ok(other) => panic!("{bad} assembled: {other:?}"),
        }
    }
}

#[test]
fn esplora_max_inflight_cli_conf_and_bounds() {
    assert_eq!(
        ready_config(["rbitcoin-node"]).esplora_max_inflight,
        rbitcoin_electrum::DEFAULT_MAX_CONNECTIONS
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--esplora-max-inflight", "1024"]).esplora_max_inflight,
        1024
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--esplora-max-inflight=8"]).esplora_max_inflight,
        8
    );
    let mut c = NodeConfig::default();
    let zero = c.apply_kv("esplora_max_inflight", "0").unwrap_err();
    assert!(format!("{zero}").contains(">= 1"), "{zero}");
    let bad = c.apply_kv("esplora_max_inflight", "nope").unwrap_err();
    assert!(format!("{bad}").contains("esplora_max_inflight"), "{bad}");
    let max = tokio::sync::Semaphore::MAX_PERMITS;
    let over = c
        .apply_kv("esplora_max_inflight", &(max + 1).to_string())
        .unwrap_err();
    assert!(format!("{over}").contains("<="), "{over}");
    let huge = c
        .apply_kv("esplora_max_inflight", "9999999999999999999")
        .unwrap_err();
    assert!(format!("{huge}").contains("esplora_max_inflight"), "{huge}");
    assert_eq!(
        c.esplora_max_inflight,
        rbitcoin_electrum::DEFAULT_MAX_CONNECTIONS,
        "a refused value leaves the default"
    );
}

#[test]
fn sv2_tp_idle_timeout_argv_default_off() {
    use std::time::Duration;
    assert_eq!(
        ready_config(["rbitcoin-node"]).sv2_tp_idle_timeout,
        Duration::ZERO
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--sv2-tp-idle-timeout", "90"]).sv2_tp_idle_timeout,
        Duration::from_secs(90)
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--sv2-tp-idle-timeout=0"]).sv2_tp_idle_timeout,
        Duration::ZERO
    );
    for bad in ["86401", "-1", "x"] {
        assert!(
            operator_config_from_args(
                ["rbitcoin-node", "--sv2-tp-idle-timeout", bad],
                &NodeEnv::default()
            )
            .is_err(),
            "{bad}"
        );
    }
}

#[test]
fn electrum_max_sp_subs_cli_conf_and_bounds() {
    assert_eq!(
        ready_config(["rbitcoin-node"]).electrum_max_sp_subs,
        rbitcoin_electrum::DEFAULT_MAX_SP_SUBS
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--electrum-max-sp-subs", "100"]).electrum_max_sp_subs,
        100
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--electrum-max-sp-subs=2"]).electrum_max_sp_subs,
        2
    );
    let mut c = NodeConfig::default();
    c.apply_kv("electrum_max_sp_subs", "40").unwrap();
    assert_eq!(c.electrum_max_sp_subs, 40);
    let zero = c.apply_kv("electrum_max_sp_subs", "0").unwrap_err();
    assert!(format!("{zero}").contains(">= 1"), "{zero}");
    let bad = c.apply_kv("electrum_max_sp_subs", "nope").unwrap_err();
    assert!(format!("{bad}").contains("electrum_max_sp_subs"), "{bad}");
    assert_eq!(
        c.electrum_max_sp_subs, 40,
        "a refused value changes nothing"
    );
}

#[test]
fn electrum_max_subs_cli_conf_and_bounds() {
    assert_eq!(
        ready_config(["rbitcoin-node"]).electrum_max_subs,
        rbitcoin_electrum::DEFAULT_MAX_SCRIPTHASH_SUBS
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--electrum-max-subs", "25000"]).electrum_max_subs,
        25_000
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--electrum-max-subs=5"]).electrum_max_subs,
        5
    );
    let mut c = NodeConfig::default();
    let zero = c.apply_kv("electrum_max_subs", "0").unwrap_err();
    assert!(format!("{zero}").contains(">= 1"), "{zero}");
    let bad = c.apply_kv("electrum_max_subs", "nope").unwrap_err();
    assert!(format!("{bad}").contains("electrum_max_subs"), "{bad}");
    assert_eq!(
        c.electrum_max_subs,
        rbitcoin_electrum::DEFAULT_MAX_SCRIPTHASH_SUBS,
        "a refused value leaves the default"
    );
}

#[test]
fn max_sh_creates_and_esplora_block_template_cli_hyphens() {
    let omitted = ready_config(["rbitcoin-node"]);
    assert_eq!(
        omitted.max_sh_creates,
        rbitcoin_query::DEFAULT_MAX_SH_CREATES
    );
    assert!(!omitted.esplora_block_template);
    let n = ready_config(["rbitcoin-node", "--max-sh-creates", "42"]);
    assert_eq!(n.max_sh_creates, 42);
    let eq = ready_config(["rbitcoin-node", "--max-sh-creates=9"]);
    assert_eq!(eq.max_sh_creates, 9);
    assert_eq!(
        omitted.heavy_sh_creates,
        rbitcoin_query::DEFAULT_HEAVY_SH_CREATES
    );
    let heavy = ready_config(["rbitcoin-node", "--heavy-sh-creates", "500"]);
    assert_eq!(heavy.heavy_sh_creates, 500);
    let gbt = ready_config(["rbitcoin-node", "--esplora-block-template"]);
    assert!(gbt.esplora_block_template);
    let gbt_eq = ready_config(["rbitcoin-node", "--esplora-block-template=1"]);
    assert!(gbt_eq.esplora_block_template);
    let off = ready_config(["rbitcoin-node", "--esplora-block-template=0"]);
    assert!(!off.esplora_block_template);
    let d = NodeConfig::default();
    assert!(!d.electrum_onion && !d.esplora_onion && !d.electrum_i2p && !d.esplora_i2p);
    let tor = [
        "rbitcoin-node",
        "--tor-control",
        "--electrum-listen",
        "--esplora-listen",
    ];
    let on = ready_config(tor.into_iter().chain(["--electrum-onion"]));
    assert!(on.electrum_onion && !on.esplora_onion && !on.electrum_i2p && !on.esplora_i2p);
    let on = ready_config(tor.into_iter().chain(["--esplora-onion=1"]));
    assert!(on.esplora_onion && !on.electrum_onion && !on.electrum_i2p && !on.esplora_i2p);
    let sam = [
        "rbitcoin-node",
        "--i2p-sam",
        "--electrum-listen",
        "--esplora-listen",
    ];
    let on = ready_config(sam.into_iter().chain(["--electrum-i2p"]));
    assert!(on.electrum_i2p && !on.esplora_i2p && !on.electrum_onion && !on.esplora_onion);
    let on = ready_config(sam.into_iter().chain(["--esplora-i2p", "--electrum-i2p=0"]));
    assert!(on.esplora_i2p && !on.electrum_i2p && !on.electrum_onion && !on.esplora_onion);
}

/// Each wallet overlay switch needs its overlay, its listener, and a TCP
/// bind that `127.0.0.1:<port>` reaches, since Tor and SAM forward there.
#[test]
fn wallet_overlay_switches_refuse_what_they_cannot_publish() {
    for (flag, key, listen, listen_key, overlay, overlay_key) in [
        (
            "electrum-onion",
            "electrum_onion",
            "--electrum-listen",
            "electrum_listen",
            "--tor-control",
            "tor_control",
        ),
        (
            "esplora-onion",
            "esplora_onion",
            "--esplora-listen",
            "esplora_listen",
            "--tor-control",
            "tor_control",
        ),
        (
            "electrum-i2p",
            "electrum_i2p",
            "--electrum-listen",
            "electrum_listen",
            "--i2p-sam",
            "i2p_sam",
        ),
        (
            "esplora-i2p",
            "esplora_i2p",
            "--esplora-listen",
            "esplora_listen",
            "--i2p-sam",
            "i2p_sam",
        ),
    ] {
        let mut c = NodeConfig::default();
        c.validate().unwrap();
        c.apply_kv(key, "1").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains(&format!("{flag} requires {overlay}")), "{err}");
        c.apply_kv(overlay_key, "").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains(&format!("{flag} requires {listen}")), "{err}");
        for unreached in [
            "10.0.0.5:50001",
            "127.0.0.2:50001",
            "[::1]:50001",
            "[::]:50001",
        ] {
            c.apply_kv(listen_key, unreached).unwrap();
            let err = c.validate().unwrap_err().to_string();
            assert!(err.contains("not reachable"), "{flag} {unreached}: {err}");
        }
        c.apply_kv(listen_key, "0.0.0.0:50001").unwrap();
        c.validate().unwrap();
        c.apply_kv(listen_key, "").unwrap();
        c.validate().unwrap();
        // Off again, any bind is fine: the switch is the only thing refused.
        c.apply_kv(listen_key, "10.0.0.5:50001").unwrap();
        c.apply_kv(key, "0").unwrap();
        c.validate().unwrap();
    }
    #[cfg(unix)]
    for key in ["esplora_onion", "esplora_i2p"] {
        let mut c = NodeConfig::default();
        c.apply_kv("tor_control", "").unwrap();
        c.apply_kv("i2p_sam", "").unwrap();
        c.apply_kv("esplora_listen", "/run/rbitcoin/esplora.sock")
            .unwrap();
        c.validate().unwrap();
        c.apply_kv(key, "1").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("unix socket"), "{key}: {err}");
    }
}

include!("overlay_config_journey.rs");

/// Testnet3 default milestone: Core v32.1 `defaultAssumeValid` at 5_128_859
/// and Core `CTestNetParams` `nMinimumChainWork`, like mainnet's 966_143.
#[test]
fn testnet3_default_milestone_is_anchored() {
    use bitcoin::hashes::Hash;
    let cfg = ready_config(["rbitcoin-node", "--network", "testnet"]);
    assert_eq!(cfg.network, Network::Testnet);
    let m = cfg.milestone();
    assert_eq!(m.height, 5_128_859);
    let anchor = m.anchor.expect("testnet3 default milestone is anchored");
    assert_eq!(
        anchor.hash.to_string(),
        "00000000b318a3703d14a844c55ef507f4c2fc8f8766e24271fd43c180c51637"
    );
    let mut min_work = [0u8; 32];
    min_work[22..].copy_from_slice(&[0x17, 0xf4, 0x9f, 0x70, 0x21, 0x47, 0xf1, 0x0c, 0x0e, 0xb6]);
    assert_eq!(anchor.min_work_be, min_work);
    assert!(cfg.meets_minimum_chain_work(min_work));
    let mut below = min_work;
    below[31] -= 1;
    assert!(!cfg.meets_minimum_chain_work(below));

    // A chain without the anchor at 5_128_859 runs scripts at any height.
    let block = [0x22u8; 32];
    let other_chain = |h: u32| (h == 100).then_some(block).or(Some([0x33; 32]));
    assert!(!m.skips_scripts_at(100));
    assert!(!m.skips_scripts(100, &block, other_chain));
    let anchored = |h: u32| match h {
        100 => Some(block),
        5_128_859 => Some(anchor.hash.to_byte_array()),
        _ => None,
    };
    assert!(
        m.skips_scripts(100, &block, anchored),
        "the anchor hash is enough; chain work below the floor still skips"
    );

    // Explicit `--milestone HEIGHT` stays height-only.
    let explicit = ready_config(["rbitcoin-node", "--network", "testnet", "--milestone", "10"]);
    assert_eq!(
        explicit.milestone(),
        rbitcoin_consensus::Milestone::height(10)
    );
}

/// An empty datadir has not reached taproot, so both flags smoke.
#[test]
fn smoke_allows_sp_tweaks_with_prune_before_taproot() {
    let dir = tmp_datadir();
    std::fs::create_dir_all(&dir).unwrap();
    let code = cli_main(
        [
            "rbitcoin-node",
            "--smoke",
            "--network",
            "regtest",
            "--datadir",
            dir.to_str().unwrap(),
            "--log-level",
            "error",
            "--no-seeds",
            "--sp-tweaks",
            "--prune-seqsigwit",
        ],
        NodeEnv::default(),
    );
    assert_exit(code, ExitCode::SUCCESS);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regtest taproot is genesis. Nothing has been dropped yet, so startup
/// still turns the index on. A watermark that does not cover a dropped
/// height is refused.
#[test]
fn pruned_datadir_at_taproot_allows_tweaks_until_a_height_is_dropped() {
    let _gate = rbitcoin_consensus::script_pool::steal_test_gate();
    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-late-tweaks");
    q.set_prune_seqsigwit(true).unwrap();
    let params = rbitcoin_consensus::ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    rbitcoin_query::accept_and_connect_block(
        &q,
        &params,
        rbitcoin_primitives::Height(0),
        &genesis,
        rbitcoin_consensus::Milestone::height(u32::MAX),
    )
    .unwrap();
    assert_eq!(q.pruneheight(), None);
    let cfg = ready_config([
        "rbitcoin-node",
        "--network",
        "regtest",
        "--sp-tweaks",
        "--prune-seqsigwit",
    ]);
    rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, params.taproot_height()).unwrap();
    assert!(q.sptweaks_enabled());
    drop(q);
    let _ = std::fs::remove_dir_all(dir.path());

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-late-tweaks-dropped");
    q.set_prune_seqsigwit(true).unwrap();
    rbitcoin_query::accept_and_connect_block(
        &q,
        &params,
        rbitcoin_primitives::Height(0),
        &genesis,
        rbitcoin_consensus::Milestone::height(u32::MAX),
    )
    .unwrap();
    q.set_pruneheight(Some(rbitcoin_primitives::Height(0)))
        .unwrap();
    let err = rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, params.taproot_height())
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("prune-seqsigwit"), "{msg}");
    assert!(msg.contains("taproot"), "{msg}");
    drop(q);
    let _ = std::fs::remove_dir_all(dir.path());
}

/// Regtest is already at taproot at genesis, and nothing is dropped on a
/// one-block chain. Both flags start, and the genesis witness stays in the
/// keep window.
#[test]
fn both_flags_on_a_short_unpruned_chain_start() {
    let _gate = rbitcoin_consensus::script_pool::steal_test_gate();
    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-late-both");
    let params = rbitcoin_consensus::ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    rbitcoin_query::accept_and_connect_block(
        &q,
        &params,
        rbitcoin_primitives::Height(0),
        &genesis,
        rbitcoin_consensus::Milestone::height(u32::MAX),
    )
    .unwrap();
    let cfg = ready_config([
        "rbitcoin-node",
        "--network",
        "regtest",
        "--sp-tweaks",
        "--prune-seqsigwit",
    ]);
    rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, params.taproot_height()).unwrap();
    assert!(q.prune_seqsigwit());
    assert!(q.sptweaks_enabled());
    assert_eq!(q.pruneheight(), None);
    assert!(q.store().path().join("seqsigwit.window/0.bin").is_file());
    drop(q);
    let _ = std::fs::remove_dir_all(dir.path());
}

/// A fresh process loads the tweak flag as off. Files on disk are still the
/// index. Nothing has been dropped on a one-block chain, so prune starts and
/// the genesis witness stays in the keep window.
#[test]
fn adding_prune_beside_existing_tweaks_on_a_short_chain_starts() {
    let _gate = rbitcoin_consensus::script_pool::steal_test_gate();
    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-tweaks-then-prune");
    let params = rbitcoin_consensus::ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    rbitcoin_query::accept_and_connect_block(
        &q,
        &params,
        rbitcoin_primitives::Height(0),
        &genesis,
        rbitcoin_consensus::Milestone::height(u32::MAX),
    )
    .unwrap();
    q.set_sptweaks_enabled(true, rbitcoin_primitives::Height(0))
        .unwrap();
    let before = q.store().txs.seqsigwit.body_logical_len();
    let path = dir.path().to_path_buf();
    drop(q);
    let q = rbitcoin_query::Query::open_or_create_tiny(&path).unwrap();
    assert!(!q.sptweaks_enabled());
    let cfg = ready_config([
        "rbitcoin-node",
        "--network",
        "regtest",
        "--sp-tweaks",
        "--prune-seqsigwit",
    ]);
    rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, params.taproot_height()).unwrap();
    assert!(q.prune_seqsigwit());
    assert!(q.sptweaks_enabled());
    assert!(path.join("seqsigwit.window/0.bin").is_file());
    assert!(q.store().txs.seqsigwit.body_logical_len() < before);
    drop(q);
    let _ = std::fs::remove_dir_all(dir.path());
}

/// Prune is on, tweak files exist, and this start left `--sp-tweaks` off.
/// Startup says how many blocks remain before that index cannot be enabled.
#[test]
fn prune_without_sp_tweaks_warns_how_many_blocks_remain() {
    let _gate = rbitcoin_consensus::script_pool::steal_test_gate();
    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-omit-tweaks");
    let params = rbitcoin_consensus::ChainParams::regtest();
    let genesis = rbitcoin_consensus::genesis_block(&params);
    rbitcoin_query::accept_and_connect_block(
        &q,
        &params,
        rbitcoin_primitives::Height(0),
        &genesis,
        rbitcoin_consensus::Milestone::height(u32::MAX),
    )
    .unwrap();
    q.set_sptweaks_enabled(true, rbitcoin_primitives::Height(0))
        .unwrap();
    let path = dir.path().to_path_buf();
    drop(q);
    let q = rbitcoin_query::Query::open_or_create_tiny(&path).unwrap();
    let cfg = ready_config(["rbitcoin-node", "--network", "regtest", "--prune-seqsigwit"]);
    rbitcoin_log::capture_logs(true);
    rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, params.taproot_height()).unwrap();
    let lines = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    assert!(!q.sptweaks_enabled());
    assert!(q.prune_seqsigwit());
    let joined = lines
        .iter()
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("287 more blocks"),
        "startup should say how many blocks remain:\n{joined}"
    );
    drop(q);
    let _ = std::fs::remove_dir_all(dir.path());
}

/// The tip flush can land before the tweak rows. Those spills are still
/// on disk while the prune height lags. Startup has to seal the gap
/// before it advances that height, or the witnesses are deleted and the
/// index can never be built.
#[test]
fn restart_seals_a_pruned_tweak_gap_before_the_prune_height_advances() {
    use rbitcoin_primitives::{Fk, Height};
    use rbitcoin_query::testutil::FixtureChain;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-tweak-gap");
    q.enter_direct_index_mode_sh(false).unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    let mut prev = Fk::NULL;
    let mut parent_hash: Option<[u8; 32]> = None;
    for h in 0..290u32 {
        let version = 1i32;
        let timestamp = h + 1;
        let bits = 0x207fffff;
        let nonce = h;
        let mut merkle = [0u8; 32];
        merkle[0..4].copy_from_slice(&h.to_le_bytes());
        merkle[4] = 0xab;
        let hash = match parent_hash {
            None => merkle,
            Some(ph) => {
                rbitcoin_store::block_header_hash(version, &ph, &merkle, timestamp, bits, nonce)
            }
        };
        let mut header = HeaderRecord {
            prev_fk: prev,
            version,
            timestamp,
            bits,
            nonce,
            merkle_root: merkle,
            hash,
            size: 0,
            weight: 0,
        };
        if h > 0 {
            header.merkle_root = {
                let mut txid = [0u8; 32];
                txid[0..4].copy_from_slice(&h.to_le_bytes());
                txid[31] = 0xcb;
                txid
            };
            let ph = parent_hash.expect("parent");
            header.hash = rbitcoin_store::block_header_hash(
                version,
                &ph,
                &header.merkle_root,
                timestamp,
                bits,
                nonce,
            );
        }
        let mut txid = [0u8; 32];
        txid[0..4].copy_from_slice(&h.to_le_bytes());
        txid[31] = 0xcb;
        let ta = rbitcoin_query::TxApply {
            tx: TxRecord {
                txid,
                version: 1,
                locktime: 0,
                input_count: 1,
                output_count: 1,
            },
            inputs: vec![InputRecord {
                prev_txid: [0u8; 32],
                parent: rbitcoin_store::InputParent::Coinbase,
                sequence: u32::MAX,
                script_sig: vec![h as u8],
                witness: vec![vec![0x11]],
            }],
            outputs: vec![OutputRecord::unspent(50_0000_0000, vec![0x51])],
        };
        parent_hash = Some(header.hash);
        prev = q.connect_block(Height(h), &header, &[ta]).unwrap();
        if h == 0 {
            q.set_sptweaks_enabled(true, Height(0)).unwrap();
            q.put_sp_tweaks_block(Height(0), prev, &[None]).unwrap();
            q.set_sptweaks_enabled(false, Height(0)).unwrap();
        }
    }
    q.join_tip_flush().unwrap();
    assert_eq!(q.tip_height(), Some(Height(289)));
    assert!(
        q.pruneheight().is_none_or(|ph| ph.0 < 1),
        "prune height must still lag the tip, got {:?}",
        q.pruneheight()
    );
    assert!(
        dir.path().join("seqsigwit.window/1.bin").is_file(),
        "the height startup would drop is still on disk"
    );
    q.store().sync_spend_durable(289).unwrap();
    let path = dir.path().to_path_buf();
    drop(q);
    let q = rbitcoin_query::Query::open_or_create_tiny(&path).unwrap();
    assert_eq!(q.tip_height(), Some(Height(289)));
    let cfg = ready_config([
        "rbitcoin-node",
        "--network",
        "regtest",
        "--sp-tweaks",
        "--prune-seqsigwit",
    ]);
    rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, 0).unwrap();
    let next = q.sptweaks_next_height().unwrap().0;
    let ph = q.pruneheight().expect("tip 289 drops height 1").0;
    assert!(next > ph, "next {next} prune {ph}");
    assert!(
        next > 289,
        "the gap through the tip was sealed, next {next}"
    );
    drop(q);
    let _ = std::fs::remove_dir_all(&path);
}

/// A damaged tweak table used to stop an unpruned node. Startup logs the
/// failure and keeps running. A prune refusal is still fatal.
#[test]
fn unpruned_tweak_enable_failure_logs_and_starts() {
    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("node-bad-tweaks");
    q.set_sptweaks_enabled(true, rbitcoin_primitives::Height(0))
        .unwrap();
    let path = dir.path().to_path_buf();
    drop(q);
    let meta = path.join("sp_tweaks.idx").join("meta");
    std::fs::write(&meta, [0u8; 4]).unwrap();
    let q = rbitcoin_query::Query::open_or_create_tiny(&path).unwrap();
    let cfg = ready_config(["rbitcoin-node", "--network", "regtest", "--sp-tweaks"]);
    rbitcoin_log::capture_logs(true);
    rbitcoin_node::run::apply_startup_index_mode(&q, &cfg, 0).unwrap();
    let lines = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    assert!(!q.sptweaks_enabled());
    let joined = lines
        .iter()
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("sp_tweaks: enable failed"),
        "startup should keep running:\n{joined}"
    );
    drop(q);
    let _ = std::fs::remove_dir_all(&path);
}

/// RPC binds only after catch-up, so a bad `--rpc-cookie-file` must fail
/// the launch rather than surface as a warning after IBD.
#[test]
fn cli_rejects_bad_rpc_cookie_at_launch() {
    let dir = tmp_datadir();
    std::fs::create_dir_all(&dir).unwrap();
    let cookie = dir.join("rpc.cookie");
    let run = |extra: &[&str]| {
        let mut args = vec![
            "rbitcoin-node",
            "--smoke",
            "--network",
            "regtest",
            "--datadir",
            dir.to_str().unwrap(),
            "--log-level",
            "error",
            "--no-seeds",
            "--milestone",
            "0",
            "--rpc-cookie-file",
            cookie.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        cli_main(args, NodeEnv::default())
    };
    assert_exit(run(&["--rpc-listen"]), ExitCode::FAILURE);
    std::fs::write(&cookie, "__cookie__:secret\n").unwrap();
    assert_exit(run(&["--rpc-listen"]), ExitCode::FAILURE);
    std::fs::write(&cookie, "__cookie__:secret").unwrap();
    // Socket-only RPC would silently ignore the cookie (message pinned in config.rs).
    assert_exit(run(&["--rpc"]), ExitCode::FAILURE);
    assert_exit(
        run(&["--rpc-listen", "--rpc-cookie-perms", "0640"]),
        ExitCode::from(2),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cookie, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_exit(run(&["--rpc-listen"]), ExitCode::FAILURE);
        std::fs::set_permissions(&cookie, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_exit(
            run(&["--rpc-listen", "--rpc-cookie-perms", "group"]),
            ExitCode::SUCCESS,
        );
        std::fs::set_permissions(&cookie, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_exit(run(&["--rpc-listen"]), ExitCode::SUCCESS);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `RBITCOIN_P2P_MAX_INBOUND` sets the inbound cap only when CLI and conf
/// omit it. `0` is no inbound slots, as `--max-inbound 0` is. Empty is unset.
#[test]
fn inbound_env_is_below_cli_and_conf() {
    use rbitcoin_node::config::DEFAULT_MAX_INBOUND;
    let inbound = |args: &[&str], val: &str| {
        let env = env_of(&[("RBITCOIN_P2P_MAX_INBOUND", val)]).unwrap();
        let args = std::iter::once("rbitcoin-node").chain(args.iter().copied());
        match operator_config_from_args(args, &env) {
            Ok(OperatorArgs::Ready { config, .. }) => config.listen.max_inbound,
            other => panic!("expected assembled config, got {other:?}"),
        }
    };
    assert_eq!(inbound(&[], "91"), 91);
    assert_eq!(inbound(&["--max-inbound", "12"], "91"), 12);
    assert_eq!(inbound(&[], "0"), 0);
    assert_eq!(inbound(&[], ""), DEFAULT_MAX_INBOUND);

    let conf_dir = tmp_datadir();
    std::fs::create_dir_all(&conf_dir).unwrap();
    let conf = conf_dir.join("rbitcoin.conf");
    std::fs::write(&conf, "max_inbound=7\n").unwrap();
    assert_eq!(inbound(&["--conf", conf.to_str().unwrap()], "91"), 7);
    let _ = std::fs::remove_dir_all(&conf_dir);
}

/// `RBITCOIN_BLOCK_QUEUE_BYTES` wins over `RBITCOIN_BLOCK_QUEUE_GB`. `0` is
/// unlimited. Empty is unset.
#[test]
fn block_queue_env_sets_the_assign_stop() {
    use rbitcoin_query::BQ_ASSIGN_STOP_BYTES;
    const BYTES: &str = "RBITCOIN_BLOCK_QUEUE_BYTES";
    const GB: &str = "RBITCOIN_BLOCK_QUEUE_GB";
    let stop = |vars: &[(&str, &str)]| {
        let env = env_of(vars).unwrap();
        match operator_config_from_args(["rbitcoin-node"], &env) {
            Ok(OperatorArgs::Ready { config, .. }) => config.bq_assign_stop_bytes,
            other => panic!("expected assembled config, got {other:?}"),
        }
    };
    assert_eq!(stop(&[]), BQ_ASSIGN_STOP_BYTES);
    assert_eq!(stop(&[(GB, "2")]), 2 << 30);
    assert_eq!(stop(&[(GB, "2"), (BYTES, "4096")]), 4096);
    assert_eq!(stop(&[(BYTES, "4096"), (GB, "2")]), 4096);
    assert_eq!(stop(&[(GB, "0")]), u64::MAX);
    assert_eq!(stop(&[(BYTES, "0"), (GB, "2")]), u64::MAX);
    assert_eq!(stop(&[(BYTES, ""), (GB, "2")]), 2 << 30);
    assert_eq!(stop(&[(GB, "")]), BQ_ASSIGN_STOP_BYTES);
}

/// `RBITCOIN_TEST_DROP_STORE`, `RBITCOIN_SH_FORCE_REBUILD`,
/// `RBITCOIN_RPC_PACKAGE_DIALECT`, and `RBITCOIN_RPC_WAIT_TIP_IDLE` are on for
/// `1` or `true` and off for `0`, `false`, or empty (`true`/`false` in any
/// case). All but the first reach `NodeConfig`.
#[test]
fn env_flags_take_one_true_zero_or_false() {
    const KEYS: [&str; 4] = [
        "RBITCOIN_TEST_DROP_STORE",
        "RBITCOIN_SH_FORCE_REBUILD",
        "RBITCOIN_RPC_PACKAGE_DIALECT",
        "RBITCOIN_RPC_WAIT_TIP_IDLE",
    ];
    let flags = |key: &str, val: &str| {
        let env = env_of(&[(key, val)]).unwrap();
        [
            env.test_drop_store,
            env.sh_force_rebuild,
            env.rpc_package_dialect,
            env.rpc_wait_tip_idle,
        ]
    };
    for on in ["1", "true", "TRUE", "True"] {
        for (i, key) in KEYS.iter().enumerate() {
            let mut want = [false; 4];
            want[i] = true;
            assert_eq!(flags(key, on), want, "{key}={on:?}");
        }
    }
    for off in ["", "0", "false", "FALSE"] {
        for key in KEYS {
            assert_eq!(flags(key, off), [false; 4], "{key}={off:?}");
        }
    }
    assert_eq!(env_of(&[]).unwrap(), NodeEnv::default());

    let env = env_of(&[
        ("RBITCOIN_SH_FORCE_REBUILD", "1"),
        ("RBITCOIN_RPC_PACKAGE_DIALECT", "1"),
        ("RBITCOIN_RPC_WAIT_TIP_IDLE", "1"),
    ])
    .unwrap();
    match operator_config_from_args(["rbitcoin-node"], &env) {
        Ok(OperatorArgs::Ready { config, .. }) => {
            assert!(config.sh_force_rebuild);
            assert!(config.rpc.package_dialect);
            assert!(config.rpc.wait_tip_idle);
        }
        other => panic!("expected assembled config, got {other:?}"),
    }
}

/// `RBITCOIN_SH_AUTO_REBUILD` defaults on. Unset or empty keeps that, `0` or
/// `false` turns it off, `1` or `true` is explicit on, and any other text
/// (`off`, `no`) stops the start like every other flag.
#[test]
fn sh_auto_rebuild_env_is_unset_on_or_off() {
    let key = "RBITCOIN_SH_AUTO_REBUILD";
    for unset in ["", " "] {
        let got = env_of(&[(key, unset)]);
        match unset {
            "" => assert_eq!(got.unwrap().sh_auto_rebuild, None),
            _ => assert!(got.is_err(), "blank is not empty"),
        }
    }
    assert_eq!(env_of(&[]).unwrap().sh_auto_rebuild, None);
    for on in ["1", "true", "TRUE"] {
        assert_eq!(env_of(&[(key, on)]).unwrap().sh_auto_rebuild, Some(true));
    }
    for off in ["0", "false", "False"] {
        assert_eq!(env_of(&[(key, off)]).unwrap().sh_auto_rebuild, Some(false));
    }
    for bad in ["off", "no", "yes", "2"] {
        let err = env_of(&[(key, bad)]).unwrap_err().to_string();
        assert!(err.contains(key), "{err}");
    }
    let assembled = |pairs: &[(&str, &str)]| match operator_config_from_args(
        ["rbitcoin-node"],
        &env_of(pairs).unwrap(),
    ) {
        Ok(OperatorArgs::Ready { config, .. }) => config.sh_auto_rebuild,
        other => panic!("expected assembled config, got {other:?}"),
    };
    assert!(assembled(&[]), "default on");
    assert!(assembled(&[(key, "1")]));
    assert!(!assembled(&[(key, "0")]));
}

/// `NodeEnv` asks its lookup for each knob by name and leaves the name match to
/// it. `std::env::var_os` ignores case on Windows, so there a lower-case name
/// still sets the knob, as it did before `NodeEnv`.
#[test]
fn env_lookup_owns_the_name_match() {
    let ignore_case = |k: &str| {
        k.eq_ignore_ascii_case("rbitcoin_sh_force_rebuild")
            .then(|| OsString::from("1"))
    };
    assert!(NodeEnv::from_lookup(ignore_case).unwrap().sh_force_rebuild);
    assert!(
        !env_of(&[("rbitcoin_sh_force_rebuild", "1")])
            .unwrap()
            .sh_force_rebuild
    );
}

/// A set `RBITCOIN_*` value that does not parse is an error naming the var,
/// not a silent default. Other env is not read.
#[test]
fn malformed_env_names_the_var() {
    for (key, val) in [
        ("RBITCOIN_P2P_MAX_INBOUND", "many"),
        ("RBITCOIN_P2P_MAX_INBOUND", "-1"),
        ("RBITCOIN_BLOCK_QUEUE_BYTES", "4k"),
        ("RBITCOIN_BLOCK_QUEUE_GB", "1.5"),
        ("RBITCOIN_TEST_DROP_STORE", "yes"),
        ("RBITCOIN_SH_FORCE_REBUILD", "on"),
        ("RBITCOIN_RPC_PACKAGE_DIALECT", " 1"),
        ("RBITCOIN_RPC_WAIT_TIP_IDLE", "ture"),
    ] {
        let err = env_of(&[(key, val)]).unwrap_err().to_string();
        assert!(
            err.contains(key) && err.contains(val),
            "{key}={val:?}: {err}"
        );
    }
    let err = env_of(&[
        ("RBITCOIN_BLOCK_QUEUE_BYTES", "4096"),
        ("RBITCOIN_BLOCK_QUEUE_GB", "lots"),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("RBITCOIN_BLOCK_QUEUE_GB"), "{err}");
    assert!(env_of(&[("PATH", "not a number"), ("RBITCOIN_NOT_A_KNOB", "x")]).is_ok());

    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = OsStr::from_bytes(&[0xff, 0x31]);
        let err =
            NodeEnv::from_lookup(|k| (k == "RBITCOIN_SH_FORCE_REBUILD").then(|| bad.to_owned()))
                .unwrap_err();
        assert!(
            err.to_string().contains("RBITCOIN_SH_FORCE_REBUILD"),
            "{err}"
        );
    }
}

/// `--trust-peer ADDR` is the only dial target. Clashing flags fail in the
/// parser, before `ensure_datadir` creates a store.
#[test]
fn trust_peer_addr() {
    use rbitcoin_node::config::P2pListen;
    use rbitcoin_node::config::DEFAULT_MAX_INBOUND;

    let h = operator_usage();
    assert!(
        h.contains("--trust-peer ADDR"),
        "help must name --trust-peer with an address"
    );

    let missing = operator_config_from_args(["rbitcoin-node", "--trust-peer"], &NodeEnv::default());
    assert!(missing.is_err(), "bare --trust-peer must not start");

    let dir = tmp_datadir();
    let addr = "192.0.2.1:8333";
    let cfg = ready_config([
        "rbitcoin-node",
        "--datadir",
        dir.to_str().unwrap(),
        "--trust-peer",
        addr,
    ]);
    assert_eq!(cfg.trust_peer.map(|a| a.to_string()).as_deref(), Some(addr));
    assert!(cfg.trust_peer_host.is_none());
    assert_eq!(cfg.listen.connect.len(), 1);
    assert_eq!(cfg.listen.connect[0].to_string(), addr);
    assert!(cfg.listen.connect_dns.is_empty());
    assert!(!cfg.listen.use_seeds);
    assert!(matches!(cfg.listen.p2p, P2pListen::Off));
    assert!(cfg.listen.p2p_extra.is_empty());
    assert!(!cfg.listen.listen_onion);
    assert!(cfg.listen.seednodes.is_empty());
    assert_eq!(cfg.listen.max_inbound, 0);
    assert!(!dir.exists(), "parser must not create the datadir");

    let plain = ready_config(["rbitcoin-node", "--datadir", dir.to_str().unwrap()]);
    assert!(plain.trust_peer.is_none());
    assert!(plain.trust_peer_host.is_none());
    assert!(plain.listen.connect.is_empty());
    assert!(plain.listen.use_seeds);
    assert!(matches!(plain.listen.p2p, P2pListen::Auto));
    assert_eq!(plain.listen.max_inbound, DEFAULT_MAX_INBOUND);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The same address may also be `--connect`. A hostname is the connect-dns row.
#[test]
fn trust_peer_same_connect_and_host() {
    use rbitcoin_node::config::P2pListen;
    let dir = tmp_datadir();
    let addr = "192.0.2.1:8333";
    let same = ready_config([
        "rbitcoin-node",
        "--datadir",
        dir.to_str().unwrap(),
        "--connect",
        addr,
        "--trust-peer",
        addr,
        "--no-listen",
        "--max-inbound",
        "0",
    ]);
    assert_eq!(same.listen.connect.len(), 1);
    assert_eq!(same.listen.connect[0].to_string(), addr);
    assert_eq!(same.listen.max_inbound, 0);

    let dns = ready_config([
        "rbitcoin-node",
        "--datadir",
        dir.to_str().unwrap(),
        "--trust-peer",
        "seed.example:8333",
    ]);
    assert!(dns.trust_peer.is_none());
    assert_eq!(dns.trust_peer_host.as_deref(), Some("seed.example:8333"));
    assert!(dns.listen.connect.is_empty());
    assert_eq!(
        dns.listen.connect_dns,
        vec!["seed.example:8333".to_string()]
    );
    assert!(!dns.listen.use_seeds);
    assert!(matches!(dns.listen.p2p, P2pListen::Off));

    let conf = dir.join("trust.conf");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&conf, "trustpeer=192.0.2.1:8333\n").unwrap();
    let from_conf = ready_config([
        "rbitcoin-node",
        "--datadir",
        dir.to_str().unwrap(),
        "--conf",
        conf.to_str().unwrap(),
    ]);
    assert_eq!(
        from_conf.trust_peer.map(|a| a.to_string()).as_deref(),
        Some(addr)
    );
    assert!(matches!(from_conf.listen.p2p, P2pListen::Off));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second address, a different connect, seeds, listen, or inbound is a start error.
#[test]
fn trust_peer_clashing_flags() {
    let dir = tmp_datadir();
    let addr = "192.0.2.1:8333";
    let clashes: &[&[&str]] = &[
        &["--trust-peer", addr, "--trust-peer", "192.0.2.2:8333"],
        &["--trust-peer", addr, "--connect", "192.0.2.2:8333"],
        &["--trust-peer", addr, "--seed-node", "192.0.2.9:8333"],
        &["--trust-peer", addr, "--listen", "127.0.0.1:8333"],
        &["--trust-peer", addr, "--listen-onion"],
        &["--trust-peer", addr, "--max-inbound", "1"],
        &["--connect", "seed.example:8333", "--trust-peer", addr],
    ];
    for extra in clashes {
        let mut args = vec!["rbitcoin-node", "--datadir", dir.to_str().unwrap()];
        args.extend_from_slice(extra);
        assert!(
            operator_config_from_args(args, &NodeEnv::default()).is_err(),
            "expected start error for {extra:?}"
        );
        assert!(!dir.exists(), "clash must not create the datadir");
    }

    let mut cfg = rbitcoin_node::config::NodeConfig::default();
    cfg.apply_kv("trust_peer", addr).unwrap();
    cfg.apply_kv("listen", "127.0.0.1:8333").unwrap();
    let err = cfg.seal_trust_peer().unwrap_err().to_string();
    assert!(err.contains("trust-peer"), "{err}");
    assert!(err.contains("--listen"), "{err}");

    let mut cfg = rbitcoin_node::config::NodeConfig::default();
    cfg.apply_kv("trustpeer", addr).unwrap();
    cfg.apply_kv("max_inbound", "8").unwrap();
    let err = cfg.seal_trust_peer().unwrap_err().to_string();
    assert!(err.contains("--max-inbound"), "{err}");

    let mut cfg = rbitcoin_node::config::NodeConfig::default();
    cfg.apply_kv("trust_peer", addr).unwrap();
    let err = cfg
        .apply_kv("trustpeer", "192.0.2.2:8333")
        .unwrap_err()
        .to_string();
    assert!(err.contains("exactly one"), "{err}");

    let mut bare = rbitcoin_node::config::NodeConfig::default();
    bare.apply_kv("trust_peer", addr).unwrap();
    let err = bare.validate().unwrap_err().to_string();
    assert!(err.contains("--listen"), "{err}");

    let mut sealed = rbitcoin_node::config::NodeConfig::default();
    sealed.apply_kv("trust_peer", addr).unwrap();
    sealed.seal_trust_peer().unwrap();
    sealed.validate().unwrap();

    // Listen fields stay writable. Validate still rejects the clashes.
    sealed.listen.p2p = rbitcoin_node::config::P2pListen::Auto;
    let err = sealed.validate().unwrap_err().to_string();
    assert!(err.contains("--listen"), "{err}");

    sealed.listen.p2p = rbitcoin_node::config::P2pListen::Off;
    sealed
        .listen
        .p2p_extra
        .push("127.0.0.1:8334".parse().unwrap());
    let err = sealed.validate().unwrap_err().to_string();
    assert!(err.contains("--listen"), "{err}");
    sealed.listen.p2p_extra.clear();

    sealed.listen.listen_onion = true;
    let err = sealed.validate().unwrap_err().to_string();
    assert!(err.contains("--listen"), "{err}");
    assert!(
        !err.contains("listen-onion requires"),
        "onion after a seal is the trust-peer listen clash: {err}"
    );

    sealed.listen.listen_onion = false;
    sealed.tor.control = None;
    sealed.listen.max_inbound = 1;
    let err = sealed.validate().unwrap_err().to_string();
    assert!(err.contains("--max-inbound"), "{err}");

    sealed.listen.max_inbound = 0;
    sealed.listen.use_seeds = true;
    let err = sealed.validate().unwrap_err().to_string();
    assert!(err.contains("--seed-node"), "{err}");

    sealed.listen.use_seeds = false;
    sealed.listen.seednodes.push("seed.example".into());
    let err = sealed.validate().unwrap_err().to_string();
    assert!(err.contains("--seed-node"), "{err}");
}

#[test]
fn regtest_pow_bits_from_argv_and_conf() {
    let bits = |cfg: &NodeConfig| cfg.chain_params().map(|p| p.regtest_pow_bits_overlay);
    assert_eq!(
        bits(&ready_config(["rbitcoin-node", "--network=regtest"])).unwrap(),
        None
    );
    for (arg, want) in [
        ("--regtest-pow-bits=0x200fffff", 0x200f_ffff),
        ("--regtest-pow-bits=200fffff", 0x200f_ffff),
    ] {
        let cfg = ready_config(["rbitcoin-node", "--network=regtest", arg]);
        assert_eq!(cfg.regtest_pow_bits, Some(want));
        assert_eq!(bits(&cfg).unwrap().map(|b| b.to_consensus()), Some(want));
    }

    let dir = tmp_datadir();
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("pow.conf");
    std::fs::write(&conf, "regtest\nregtest_pow_bits=0x1f7fffff\n").unwrap();
    let cfg = ready_config(["rbitcoin-node", "--conf", conf.to_str().unwrap()]);
    assert_eq!(cfg.regtest_pow_bits, Some(0x1f7f_ffff));
    assert!(cfg.chain_params().is_ok());

    // Off regtest, or with a value that is not a harder regtest target.
    let off = ready_config([
        "rbitcoin-node",
        "--network=signet",
        "--regtest-pow-bits=0x200fffff",
    ]);
    let err = off.chain_params().unwrap_err().to_string();
    assert!(err.contains("regtest only"), "{err}");
    let easy = ready_config([
        "rbitcoin-node",
        "--network=regtest",
        "--regtest-pow-bits=0x2100ffff",
    ]);
    assert!(easy.chain_params().is_err());
    assert!(operator_config_from_args(
        ["rbitcoin-node", "--regtest-pow-bits=nothex"],
        &NodeEnv::default()
    )
    .is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn store_read_budget_formula_and_flag() {
    assert_eq!(store_read_budget(true, 4, None), 1);
    assert_eq!(store_read_budget(true, 4, Some(0)), 1);
    assert_eq!(store_read_budget(false, 4, None), 3);
    assert_eq!(store_read_budget(false, 4, Some(0)), 3);
    assert_eq!(store_read_budget(false, 1, None), 1);
    assert_eq!(store_read_budget(true, 64, Some(8)), 8);
    assert_eq!(store_read_budget(false, 64, Some(8)), 8);

    assert_eq!(ready_config(["rbitcoin-node"]).store_read_budget, None);
    assert_eq!(
        ready_config(["rbitcoin-node", "--store-read-budget", "0"]).store_read_budget,
        Some(0)
    );
    assert_eq!(
        ready_config(["rbitcoin-node", "--store-read-budget=12"]).store_read_budget,
        Some(12)
    );
    let mut c = NodeConfig::default();
    c.apply_kv("store_read_budget", "0").unwrap();
    assert_eq!(c.store_read_budget, Some(0));
    c.apply_kv("store_read_budget", "7").unwrap();
    assert_eq!(c.store_read_budget, Some(7));
    let bad = c.apply_kv("store_read_budget", "nope").unwrap_err();
    assert!(format!("{bad}").contains("store_read_budget"), "{bad}");
    let neg = c.apply_kv("store_read_budget", "-3").unwrap_err();
    assert!(format!("{neg}").contains("store_read_budget"), "{neg}");
    assert!(operator_config_from_args(
        ["rbitcoin-node", "--store-read-budget=-1"],
        &NodeEnv::default()
    )
    .is_err());
    let h = operator_usage();
    assert!(h.contains("--store-read-budget"), "{h}");
    assert!(
        h.contains("minimum 1"),
        "help must state the default formula: {h}"
    );
}

#[test]
fn regtest_retarget_from_argv_and_conf() {
    let retarget = |cfg: &NodeConfig| cfg.chain_params().map(|p| p.regtest_retarget);
    let regtest = ready_config(["rbitcoin-node", "--network=regtest"]);
    assert_eq!(retarget(&regtest).unwrap(), None);
    assert!(regtest.chain_params().unwrap().no_pow_retargeting());

    // Either key turns it on; the other keeps mainnet's value.
    for (args, spacing, interval) in [
        (vec!["--regtest-block-time=10"], 10, 2016),
        (vec!["--regtest-retarget-interval=60"], 600, 60),
        (
            vec!["--regtest-block-time=10", "--regtest-retarget-interval=60"],
            10,
            60,
        ),
    ] {
        let mut argv = vec!["rbitcoin-node", "--network=regtest"];
        argv.extend(args);
        let params = ready_config(argv).chain_params().unwrap();
        assert!(params.regtest_retarget.is_some());
        assert!(!params.no_pow_retargeting());
        assert!(!params.allow_min_difficulty_blocks());
        assert_eq!(params.btc.pow_target_spacing, spacing);
        assert_eq!(params.difficulty_adjustment_interval(), interval);
    }

    let dir = tmp_datadir();
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("retarget.conf");
    std::fs::write(
        &conf,
        "regtest\nregtest_pow_bits=0x1e07a120\nregtest_block_time=10\nregtest_retarget_interval=60\n",
    )
    .unwrap();
    let cfg = ready_config(["rbitcoin-node", "--conf", conf.to_str().unwrap()]);
    assert_eq!(cfg.regtest_block_time, Some(10));
    assert_eq!(cfg.regtest_retarget_interval, Some(60));
    let start = retarget(&cfg).unwrap().unwrap().start_bits;
    assert_eq!(start.to_consensus(), 0x1e07_a120);

    for (args, want) in [
        (
            vec!["--network=signet", "--regtest-block-time=10"],
            "regtest only",
        ),
        (
            vec!["--network=regtest", "--regtest-block-time=0"],
            "greater than zero",
        ),
        (
            vec!["--network=regtest", "--regtest-retarget-interval=1"],
            "at least 2",
        ),
        // Below 4 s the lower clamp floors to zero.
        (
            vec![
                "--network=regtest",
                "--regtest-block-time=1",
                "--regtest-retarget-interval=3",
            ],
            "at least 4 seconds",
        ),
        (
            vec![
                "--network=regtest",
                "--regtest-block-time=4294967295",
                "--regtest-retarget-interval=2",
            ],
            "too long",
        ),
        // Target 1 is harder than the floor of a 4 s timespan, target 4.
        (
            vec![
                "--network=regtest",
                "--regtest-pow-bits=0x01010000",
                "--regtest-block-time=1",
                "--regtest-retarget-interval=4",
            ],
            "harder than",
        ),
        // The pow limit is easier than the ceiling four timespans allow.
        (
            vec![
                "--network=regtest",
                "--regtest-pow-bits=0x2000ffff",
                "--regtest-block-time=10",
            ],
            "ceiling",
        ),
    ] {
        let mut argv = vec!["rbitcoin-node"];
        argv.extend(args);
        let err = ready_config(argv).chain_params().unwrap_err().to_string();
        assert!(err.contains(want), "{err}");
    }
    assert!(operator_config_from_args(
        ["rbitcoin-node", "--regtest-block-time=soon"],
        &NodeEnv::default()
    )
    .is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Help and version are requested output: stdout, exit 0, nothing on stderr.
#[test]
fn help_and_version_print_to_stdout() {
    for (flag, want) in [
        ("--help", "usage:"),
        ("-h", "usage:"),
        ("--version", env!("CARGO_PKG_VERSION")),
        ("-V", env!("CARGO_PKG_VERSION")),
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_rbitcoin-node"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag} must exit 0");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains(want), "{flag} stdout: {stdout:?}");
        assert!(out.stderr.is_empty(), "{flag} wrote to stderr");
    }
}

#[test]
fn help_names_ibd_download_defaults() {
    let h = operator_usage();
    let want = format!(
        "up to {} concurrent getdata, max {} in transit per peer",
        rbitcoin_ibd::DEFAULT_IBD_WINDOW,
        rbitcoin_ibd::DEFAULT_BLOCKS_IN_TRANSIT_PER_PEER,
    );
    assert!(h.contains(&want), "help must say {want:?}");
}

/// A reader that already closed stdout is not an error: exit 0, no panic.
#[test]
fn help_and_version_to_a_closed_stdout_exit_zero() {
    for args in [&["--help"][..], &["-h"], &["--version"], &["-V"]] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_rbitcoin-node"))
            .args(args)
            .stdout(writer)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{args:?}: {:?} {stderr}", out.status);
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
    }
}
