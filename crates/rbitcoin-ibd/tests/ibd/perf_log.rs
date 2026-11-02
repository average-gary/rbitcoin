use rbitcoin_ibd::ibd::confirm::ConfirmPipelineSizes;
use rbitcoin_ibd::ibd::perf_log::*;
use rbitcoin_ibd::ibd::state::WorkStructureSizes;
use rbitcoin_ibd::ibd::status::LoopStats;
use rbitcoin_log::Level;
use rbitcoin_query::ProcessOwnedSizes;
use std::sync::atomic::Ordering;

#[test]
fn ibd_perf_full_sample_is_one_timestamped_json_debug_line() {
    let s = IbdPerfSample {
        inflight: 7,
        ..Default::default()
    };
    let mut schedule = PerfLogSchedule::default();
    rbitcoin_log::capture_logs(true);
    log_sample(&s, &mut schedule);
    let logs = rbitcoin_log::take_logs();
    rbitcoin_log::capture_logs(false);
    let meters: Vec<_> = logs
        .iter()
        .filter(|(_, m)| m.starts_with("ibd: perf_full "))
        .collect();
    assert_eq!(meters.len(), 1, "{logs:?}");
    let (level, msg) = meters[0];
    assert_eq!(*level, Level::Debug, "{msg}");
    let json = msg
        .strip_prefix("ibd: perf_full ")
        .unwrap_or_else(|| panic!("{msg}"));
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {json}"));
    let obj = v.as_object().unwrap_or_else(|| panic!("{json}"));
    assert_eq!(obj.get("inflight"), Some(&serde_json::json!(7)));
    assert_eq!(obj.get("peers"), Some(&serde_json::json!(0)));
    assert!(obj.get("ts").and_then(|t| t.as_u64()).is_some(), "{json}");
    for name in SAMPLE_FIELDS.split(',') {
        assert!(obj.contains_key(name), "missing {name}");
    }
    assert_eq!(
        obj.len(),
        SAMPLE_FIELDS.split(',').count() + 1,
        "ts plus every sample field"
    );
    assert!(obj["write"].get("class_a_ms").is_some());
    assert!(obj["spend"].get("spend_ckpt_n").is_some());
    assert!(obj["spend"].get("spend_ckpt_max_ms").is_some());
    assert!(obj["spend"].get("spend_notes_ram").is_some());
    assert!(obj["work"]["body"].get("known").is_some());
    assert!(obj["owned"]["head"].get("class_a_n").is_some());
    assert!(obj["conf_pipe"].get("load_batches").is_some());
    assert!(obj["conf_pipe"].get("undrained_bytes").is_some());
    assert!(obj["live"].is_null());
    let progress = rbitcoin_ibd::ibd::progress::format_progress_line(
        &rbitcoin_ibd::ibd::progress::ProgressLineInput {
            pct: 1,
            tip: 2,
            tip_rate: 0.0,
            tip_hole: 3,
            peers: 4,
            conf_q: "ready=0".to_string(),
            txs: 5,
            horizon: 6,
            eta: "eta=n/a".to_string(),
            bq_bytes: 0,
            bq_count: 0,
            bq_soft_stop: 0,
        },
    );
    assert!(progress.starts_with("ibd: progress "), "{progress}");
    assert!(!progress.contains('{'), "{progress}");
}

#[test]
fn ibd_perf_compact_line_is_the_5s_headline() {
    let s = IbdPerfSample {
        inflight: 7,
        peers: 8,
        hole: 2,
        spend: SpendWindow {
            spend_ckpt_n: 1,
            spend_ckpt_max_ms: 12,
            spend_notes_ram: 34,
        },
        ..Default::default()
    };
    let v: serde_json::Value = serde_json::from_str(&perf_sample_json_compact(&s)).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.get("inflight"), Some(&serde_json::json!(7)));
    assert_eq!(obj.get("peers"), Some(&serde_json::json!(8)));
    assert_eq!(obj.get("hole"), Some(&serde_json::json!(2)));
    assert!(obj.get("ts").and_then(|t| t.as_u64()).is_some());
    assert!(obj.get("write").is_none(), "{obj:?}");
    assert_eq!(obj["spend"]["spend_ckpt_n"], serde_json::json!(1));
}

#[test]
fn perf_log_schedule_is_compact_between_full_ticks() {
    let base = std::time::Instant::now();
    let mut schedule = PerfLogSchedule::default();
    assert_eq!(schedule.next_at(base, false), PerfLineKind::Full);
    assert_eq!(
        schedule.next_at(base + std::time::Duration::from_secs(5), false),
        PerfLineKind::Compact
    );
    assert_eq!(
        schedule.next_at(base + std::time::Duration::from_secs(30), false),
        PerfLineKind::Full
    );
    assert_eq!(
        schedule.next_at(base + std::time::Duration::from_secs(31), true),
        PerfLineKind::Full
    );
}

#[test]
fn slow_confirm_warns_then_recovers() {
    let base = std::time::Instant::now();
    let mut slow = SlowPhaseWarn::default();
    assert_eq!(slow.on_tick(base, false), None);
    assert_eq!(slow.on_tick(base, true), Some(SlowPhaseAction::Warn));
    assert_eq!(
        slow.on_tick(base + std::time::Duration::from_secs(5), true),
        None
    );
    assert_eq!(
        slow.on_tick(base + std::time::Duration::from_secs(60), true),
        Some(SlowPhaseAction::Warn)
    );
    assert_eq!(
        slow.on_tick(base + std::time::Duration::from_secs(61), false),
        Some(SlowPhaseAction::Recovered { slow_s: 61 })
    );
}

#[test]
fn ibd_perf_sample_reports_the_spend_window() {
    let s = IbdPerfSample {
        spend: SpendWindow {
            spend_ckpt_n: 2,
            spend_ckpt_max_ms: 123,
            spend_notes_ram: 456,
        },
        ..Default::default()
    };
    let v: serde_json::Value = serde_json::from_str(&perf_sample_json(&s)).unwrap();
    assert_eq!(v["spend"]["spend_ckpt_n"], serde_json::json!(2));
    assert_eq!(v["spend"]["spend_ckpt_max_ms"], serde_json::json!(123));
    assert_eq!(v["spend"]["spend_notes_ram"], serde_json::json!(456));
}

const SAMPLE_FIELDS: &str = "inflight,inflight_cap,bq_bytes,bq_count,bq_soft_stop,buf_ahead,hole,peers,headers_done,confirm_ms,confirm_blocks,confirm_reject_stops,confirm_us_per_block,assign_ms,assign_issued,drain_ms,drain_events,status_scan_ms,dominant,live,phase_blks,connect_ms,script_ms,idx_asm_ms,write,ensure_res_hit,ensure_cold_n,pins_take_ms,pins_map_ms,asm_prevout_ms,asm_sigop_ms,asm_final_ms,asm_job_ms,asm_in_n,asm_prev_batch_n,asm_prev_same_n,asm_prev_cold_n,asm_cold_null_fk_n,asm_cold_not_pin_n,asm_cold_txid_mismatch_n,asm_cold_vout_miss_n,strong_ms,structural_spent_ms,spent_abs_ms,spent_strong_ms,spent_cold_ms,spent_pending_ms,structural_create_h_ms,structural_bip68_ms,spend_ranged,ann_ms,ann_n,ann_pread_skip,ann_sync_ms,spend_replay_ms,spend_room_wait_ms,spend,meta_ms,meta_n,meta_read_kb,meta_kept_kb,load_ms,prep_wire_arc_ms,prep_struct_ms,prep_header_ms,prep_header_skip_n,prep_prepare_ms,prep_filter_plan_ms,milestone_gate_ns,sh_runs,sh_collect_ms,sh_sort_ms,sh_seed_ms,sh_body_ms,sh_head_ms,sh_collect_pin,sh_collect_cold,load_win_ms,load_blocks,load_utxo_parents,load_parent_unique,load_pin_cache_body,load_pin_plan,load_pin_new,load_pin_body_ms,load_plan_pin_ms,load_pin_range_fill_ms,load_pin_contract_ms,load_cold_io_ms,load_cold_range_ms,load_cold_range_n,load_cold_range_body_ms,load_cold_range_decode_ms,load_cold_range_extend_n,load_cold_range_body_sqe_n,load_cold_range_guess_full_n,load_cold_range_read_kb,load_cold_range_kept_kb,load_body_tx_reads,conf_ready,conf_script_q,conf_write_q,conf_script_q_cap,conf_write_q_cap,conf_script_q_hwm,conf_write_q_hwm,thr_lookup_claim_ms,thr_lookup_stamp_ms,thr_lookup_other_ms,thr_lookup_send_wait_ms,stamp_struct_ms,stamp_struct_txid_ms,stamp_struct_walk_ms,stamp_prepare_ms,stamp_filter_ms,stamp_batch_ms,thr_load_recv_wait_ms,thr_load_pack_ms,thr_load_clone_ms,thr_load_stamp_ms,thr_load_pin_ms,thr_load_asm_ms,thr_load_prune_ms,thr_load_reject_ms,thr_load_send_wait_ms,script_jobs,script_skip,thr_script_recv_wait_ms,thr_script_work_ms,thr_script_send_wait_ms,thr_write_recv_wait_ms,thr_write_work_ms,lookup_decode_ms,lookup_precompute_ms,lookup_wave_head_ms,lookup_wave_head_probe_ms,lookup_wave_head_io_ms,lookup_wave_head_preads,lookup_wave_spent_ms,lookup_undrained_skip,lookup_blocks,lookup_ms,lookup_collect_ms,lookup_head_ms,lookup_cold_io_ms,plan_parents,plan_already,plan_cold,plan_same_batch,load_thin_ms,load_thin_edges_ms,load_thin_fill_ms,load_thin_reject_ms,load_parent_pin_ms,arch_ext_need,arch_head_need,arch_head_hit,leftover_pend,leftover_cdf0_pct,leftover_cdf3_pct,leftover_age_n,arch_pin_txid,arch_pin_txid_ms,arch_recent_n,arch_recent_ms,arch_batch_stamp,arch_resolve_ns,arch_resolve_blocks,arch_prep_assign_ms,arch_prep_collect_ms,arch_prep_inflight_ms,arch_prep_head_ms,arch_prep_head_fk_ms,arch_prep_head_keys,arch_prep_head_cands,arch_prep_hit_rank_avg_x100,arch_prep_hit_rank_n,arch_prep_miss_peeks,arch_prep_pending_hits,arch_prep_age_cdf0_pct,arch_prep_age_cdf3_pct,arch_prep_age_hit_n,arch_prep_body_lookups,arch_prep_stamp_ms,arch_prep_finish_ms,arch_write_total_ms,arch_write_reserve_ms,arch_write_body_ms,arch_write_head_ms,arch_write_spend_ms,arch_write_htxs_ms,arch_write_txstat_ms,arch_write_flush_ms,arch_write_blocks,rss_kb,rss_anon_kb,rss_file_kb,vm_hwm_kb,work,owned,conf_pipe,lookup_faults";

#[test]
fn write_inventory_stage_ms_sums_the_table() {
    let write = WriteStageSample {
        class_a_ms: 1,
        ensure_ms: 2,
        structural_ms: 3,
        class_c_ms: 4,
        sh_ms: 5,
        utxo_ms: 6,
        pins_ms: 8,
        head_sub_ms: 9,
        class_c_join_ms: 10,
        drain_join_ms: 11,
        dequeue_ms: 12,
        idx_put_ms: 13,
        ..Default::default()
    };
    assert_eq!(write.stage_ms(), 84);
    let s = IbdPerfSample {
        write: write.clone(),
        ..Default::default()
    };
    assert_eq!(write_stage_ms(&s), 84);
    let s = IbdPerfSample {
        write,
        load_ms: 30,
        connect_ms: 8,
        ..Default::default()
    };
    assert_eq!(load_stage_wall_ms(&s), 38);
}

#[cfg(target_os = "linux")]
#[test]
fn fill_rss_from_status_and_smaps_edges() {
    let mut r = ProcessRss::default();
    fill_rss_from_status(
        &mut r,
        "Name:\trbitcoin\nVmRSS:\t  1024 kB\nVmHWM:\t  2048 kB\nRssAnon:\t512 kB\nRssFile:\t256 kB\nRssShmem:\t128 kB\n",
    );
    assert_eq!(r.rss_kb, 1024);
    assert_eq!(r.hwm_kb, 2048);
    assert_eq!(r.anon_kb, 512);
    assert_eq!(r.file_kb, 384);

    let mut r = ProcessRss::default();
    fill_rss_from_smaps_rollup(
        &mut r,
        "Rss:\t  800 kB\nAnonymous:\t  300 kB\nRssAnon:\t  1 kB\nRssFile:\t  2 kB\nLocked:\t  16 kB\n",
    );
    assert_eq!(r.rss_kb, 800);
    assert_eq!(r.anon_kb, 300);
    assert_eq!(r.file_kb, 2);
    assert_eq!(r.locked_kb, 16);

    let mut r = ProcessRss::default();
    fill_rss_from_smaps_rollup(&mut r, "Rss:\t  900 kB\nAnonymous:\t  400 kB\n");
    assert_eq!(r.rss_kb, 900);
    assert_eq!(r.anon_kb, 400);
    assert_eq!(r.file_kb, 500);
}

#[test]
fn rss_cadence_reads_deep_once_per_interval() {
    let base = std::time::Instant::now();
    let mut cadence = RssCadence::default();
    assert!(cadence.deep_due(base));
    assert!(!cadence.deep_due(base + std::time::Duration::from_secs(5)));
    assert!(!cadence.deep_due(base + std::time::Duration::from_secs(29)));
    assert!(cadence.deep_due(base + std::time::Duration::from_secs(30)));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn read_platform_rss_reports_resident_size() {
    let r = read_platform_rss();
    assert!(r.rss_kb > 0, "expected a resident size, got {r:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn read_platform_rss_splits_anon_and_file_on_linux() {
    let r = read_platform_rss();
    assert!(r.hwm_kb >= r.rss_kb, "hwm should not trail rss, got {r:?}");
    assert!(
        r.anon_kb > 0 || r.file_kb > 0,
        "expected anon/file split from status or smaps_rollup, got {r:?}"
    );
    assert!(r.anon_kb <= r.rss_kb.saturating_add(256), "{r:?}");
    assert!(r.file_kb <= r.rss_kb.saturating_add(256), "{r:?}");
    let sum = r.anon_kb.saturating_add(r.file_kb);
    let skew = sum.abs_diff(r.rss_kb);
    assert!(
        skew <= 1024,
        "anon+file should ≈ rss (±1MiB): sum={sum} skew={skew} {r:?}"
    );
}

#[test]
fn sample_pulls_atomics() {
    let loop_stats = LoopStats::default();
    loop_stats.confirm_ns.store(2_000_000, Ordering::Relaxed);
    loop_stats.confirm_blocks.store(1, Ordering::Relaxed);
    loop_stats.assign_issued.store(7, Ordering::Relaxed);
    let s = sample(
        &loop_stats,
        4,
        256,
        (0, 0, 256),
        100,
        1,
        8,
        true,
        0,
        0,
        0,
        (0, 0),
        1,
        WorkStructureSizes::default(),
        ProcessOwnedSizes::default(),
        ConfirmPipelineSizes::default(),
        read_platform_rss(),
        &rbitcoin_query::ConfirmStats::default(),
    );
    assert_eq!(s.inflight, 4);
    assert_eq!(s.peers, 8);
    assert!(s.headers_done);
    assert_eq!(s.assign_issued, 7);
    assert_eq!(s.confirm_blocks, 1);
    assert_eq!(s.sh_runs, 1);
    assert_eq!(s.conf_ready, 0);
    let mut schedule = PerfLogSchedule::default();
    log_sample(&s, &mut schedule);
    let mut slow = s;
    slow.phase_blks = 1;
    slow.write.class_c_ms = 2000;
    slow.write.sh_ms = 2000;
    slow.load_ms = 6000;
    slow.write.class_a_ms = 6000;
    log_sample(&slow, &mut schedule);
}

#[test]
fn owned_sizes_json_omits_prep_ahead_inflight() {
    // Load no longer publishes prep-ahead pins. The sizes line must not
    // print those counters as zero.
    #[derive(serde::Serialize)]
    struct Wrap {
        #[serde(serialize_with = "ser_process_owned")]
        owned: ProcessOwnedSizes,
    }
    let v = serde_json::to_value(Wrap {
        owned: ProcessOwnedSizes::default(),
    })
    .unwrap();
    let obj = v["owned"].as_object().unwrap();
    assert!(obj.get("inflight_layers").is_none(), "{obj:?}");
    assert!(obj.get("inflight_pins").is_none(), "{obj:?}");
    assert!(obj.get("inflight_bytes").is_none(), "{obj:?}");
    assert!(obj.contains_key("wloc_packs"), "{obj:?}");
}

/// The heap meters print under `owned.heap`, one byte count each.
#[test]
fn owned_sizes_json_prints_the_heap_meters() {
    #[derive(serde::Serialize)]
    struct Wrap {
        #[serde(serialize_with = "ser_process_owned")]
        owned: ProcessOwnedSizes,
    }
    let heap = rbitcoin_query::HeapMeters {
        pin_est: 1,
        pin_maps: 2,
        decoded_blocks: 3,
        decoded: 4,
        spend_notes: 5,
        spend_dir: 6,
        conf_plans: 7,
    };
    let v = serde_json::to_value(Wrap {
        owned: ProcessOwnedSizes {
            heap,
            ..ProcessOwnedSizes::default()
        },
    })
    .unwrap();
    assert_eq!(
        v["owned"]["heap"],
        serde_json::json!({
            "pin_est": 1,
            "pin_maps": 2,
            "decoded_blocks": 3,
            "decoded": 4,
            "spend_notes": 5,
            "spend_dir": 6,
            "conf_plans": 7
        })
    );
}

#[test]
fn tip_perf_json_is_timestamped() {
    let line = tip_perf_json(&TipPerfLog {
        rss: ProcessRss {
            rss_kb: 2048,
            anon_kb: 1024,
            file_kb: 512,
            hwm_kb: 3072,
            locked_kb: 0,
        },
        cache_bodies: 3,
        held_bodies: 1,
        mp_live: 4,
        follow_live: 5,
        blocks: 6,
        accepts: 7,
        rejects: 1,
        accept_avg_us: 9,
        accept_max_us: 10,
        accept_lock_us: 11,
        accept_utxo_us: 12,
        accept_script_us: 13,
        accept_durable_us: 14,
        inv_tx: 15,
        getdata_tx: 16,
        announce: 17,
        esplora_n: 18,
        esplora_avg_us: 19,
        esplora_max_us: 20,
        electrum_n: 21,
        electrum_avg_us: 22,
        electrum_max_us: 23,
        serve_n: 24,
        serve_bytes: 25,
        serve_tx: 26,
        serve_avg_us: 27,
        serve_max_us: 28,
        sv2_checks: 29,
        sv2_builds: 2,
        sv2_build_avg_us: 30,
        sv2_build_max_us: 31,
        sv2_validating: 3,
    });
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(v["ts"].as_u64().unwrap() > 0, "{line}");
    assert_eq!(v["rss"]["rss_kb"], 2048);
    assert_eq!(v["accepts"], 7);
    assert_eq!(v["serve_n"], 24);
    assert_eq!(v["blocks"], 6);
    assert_eq!(v["mp_live"], 4);
    assert_eq!(v["sv2_checks"], 29);
    assert_eq!(v["sv2_builds"], 2);
    assert_eq!(v["sv2_build_avg_us"], 30);
    assert_eq!(v["sv2_build_max_us"], 31);
    assert_eq!(v["sv2_validating"], 3);
}
