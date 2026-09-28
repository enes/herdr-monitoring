use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

use super::collector::Collector;
use super::source::{ProcessMeta, ProcessReading};
use super::*;

fn process_meta(pid: u32, ppid: u32, name: &str, cmdline: &str) -> ProcessMeta {
    ProcessMeta {
        pid,
        ppid,
        name: name.into(),
        cmdline: cmdline.into(),
        children: Vec::new(),
    }
}

fn topology() -> Vec<ProcessMeta> {
    vec![
        process_meta(10, 1, "root", "root"),
        process_meta(20, 10, "child", "child"),
        process_meta(30, 20, "grandchild", "grandchild"),
        process_meta(40, 1, "other", "other"),
    ]
}

fn reading(cpu: f64, started: Option<u64>, rss: u64) -> ProcessReading {
    ProcessReading {
        cpu_time: cpu,
        started: started.map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        rss,
    }
}

fn sample(
    aggregate: &mut Collector,
    topology: Vec<ProcessMeta>,
    roots: &[u32],
    excluded: &[u32],
    now: Instant,
    read: impl FnMut(u32) -> Option<ProcessReading>,
) -> MetricsSnapshot {
    aggregate.sample(topology, roots, excluded, now, read)
}

#[test]
fn one_process_warms_up_then_reports_raw_core_cpu_and_resident_bytes() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    let processes = || vec![process_meta(10, 1, "task", "task")];
    let first = sample(&mut aggregate, processes(), &[10], &[], now, |_| {
        Some(reading(1.0, Some(100), 1024))
    });
    let first_totals = first.totals(&[10]);
    assert_eq!(first_totals.own, first_totals.tree);
    assert_eq!(first_totals.tree.cpu_percent, None);
    assert_eq!(first_totals.tree.rss_bytes, Some(1024));
    assert_eq!(first_totals.tree.process_count, 1);
    let second = sample(
        &mut aggregate,
        processes(),
        &[10],
        &[],
        now + Duration::from_secs(2),
        |_| Some(reading(4.0, Some(100), 2048)),
    );
    assert_eq!(second.totals(&[10]).tree.cpu_percent, Some(150.0));
    assert_eq!(second.totals(&[10]).tree.rss_bytes, Some(2048));
}

#[test]
fn explicit_roots_include_non_agents_and_own_differs_from_child_tree() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    for (offset, cpu) in [(0, 1.0), (2, 2.0)] {
        let snapshot = sample(
            &mut aggregate,
            topology(),
            &[10],
            &[],
            now + Duration::from_secs(offset),
            |_| Some(reading(cpu, Some(100), 100)),
        );
        let totals = snapshot.totals(&[10]);
        assert_eq!(totals.own.process_count, 1);
        assert_eq!(totals.tree.process_count, 3);
        assert_eq!(totals.own.rss_bytes, Some(100));
        assert_eq!(totals.tree.rss_bytes, Some(300));
        assert_eq!(snapshot.process_ids(&[10]), vec![10, 20, 30]);
        assert_eq!(snapshot.processes[&20].ppid, 10);
        assert_eq!(snapshot.processes[&20].children, vec![30]);
        assert!(!snapshot.processes.contains_key(&40));
        if offset > 0 {
            assert_eq!(totals.own.cpu_percent, Some(50.0));
            assert_eq!(totals.tree.cpu_percent, Some(150.0));
        }
    }
}

#[test]
fn overlapping_and_repeated_roots_are_read_once_and_never_double_counted() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    for (offset, cpu) in [(0, 1.0), (1, 2.0)] {
        let mut reads = HashMap::<u32, usize>::new();
        let snapshot = sample(
            &mut aggregate,
            topology(),
            &[20, 10, 40, 10],
            &[],
            now + Duration::from_secs(offset),
            |pid| {
                *reads.entry(pid).or_default() += 1;
                Some(reading(cpu, Some(100), 100))
            },
        );
        assert_eq!(reads.len(), 4);
        assert!(reads.values().all(|count| *count == 1));
        let all = snapshot.totals(&[20, 10, 40, 10]);
        assert_eq!(all.own.process_count, 3);
        assert_eq!(all.own.rss_bytes, Some(300));
        assert_eq!(all.tree.process_count, 4);
        assert_eq!(all.tree.rss_bytes, Some(400));
        assert_eq!(snapshot.totals(&[20]).tree.process_count, 2);
        if offset > 0 {
            assert_eq!(all.own.cpu_percent, Some(300.0));
            assert_eq!(all.tree.cpu_percent, Some(400.0));
            assert_eq!(snapshot.totals(&[20]).tree.cpu_percent, Some(200.0));
        }
    }
}

#[test]
fn child_root_measured_before_its_higher_pid_parent_still_has_one_baseline() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    for (offset, cpu) in [(0, 1.0), (1, 2.0)] {
        let snapshot = sample(
            &mut aggregate,
            vec![
                process_meta(10, 40, "child", "child"),
                process_meta(40, 1, "parent", "parent"),
            ],
            &[10, 40],
            &[],
            now + Duration::from_secs(offset),
            |_| Some(reading(cpu, Some(100), 100)),
        );
        assert_eq!(snapshot.totals(&[40]).tree.process_count, 2);
        if offset > 0 {
            assert_eq!(snapshot.totals(&[40]).tree.cpu_percent, Some(200.0));
        }
    }
}

#[test]
fn monitor_subtree_is_pruned_even_when_inside_a_requested_tree() {
    let mut aggregate = Collector::default();
    let snapshot = sample(
        &mut aggregate,
        topology(),
        &[10, 40],
        &[20],
        Instant::now(),
        |pid| {
            assert!(pid != 20 && pid != 30);
            Some(reading(1.0, Some(100), 100))
        },
    );
    assert_eq!(snapshot.process_ids(&[10, 40]), vec![10, 40]);
    assert_eq!(snapshot.totals(&[10, 40]).tree.rss_bytes, Some(200));
    assert_eq!(snapshot.processes[&10].children, Vec::<u32>::new());
    assert_eq!(snapshot.totals(&[20]).missing_roots, vec![20]);
}

#[test]
fn unreadable_child_is_counted_but_not_misrepresented_as_zero_memory() {
    let mut aggregate = Collector::default();
    let snapshot = sample(
        &mut aggregate,
        topology(),
        &[10],
        &[],
        Instant::now(),
        |pid| (pid != 20).then(|| reading(1.0, Some(100), 100)),
    );
    let totals = snapshot.totals(&[10]);
    assert_eq!(totals.own.rss_bytes, Some(100));
    assert_eq!(totals.tree.rss_bytes, None);
    assert_eq!(totals.tree.process_count, 3);
    assert_eq!(snapshot.processes[&20].rss_bytes, None);
    assert_eq!(snapshot.processes[&20].cpu_percent, None);
}

#[test]
fn disappeared_root_and_unobserved_pid_report_missing() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    sample(&mut aggregate, topology(), &[10], &[], now, |_| {
        Some(reading(1.0, Some(100), 100))
    });
    let gone = sample(
        &mut aggregate,
        vec![],
        &[10, u32::MAX],
        &[],
        now + Duration::from_secs(1),
        |_| panic!("dead roots must not be read"),
    );
    assert_eq!(gone.missing_roots, vec![10, u32::MAX]);
    let totals = gone.totals(&[10]);
    assert_eq!(totals.tree.process_count, 0);
    assert_eq!(totals.tree.cpu_percent, None);
    assert_eq!(totals.tree.rss_bytes, None);
    let returned = sample(
        &mut aggregate,
        topology(),
        &[10],
        &[],
        now + Duration::from_secs(2),
        |_| Some(reading(8.0, Some(100), 100)),
    );
    assert_eq!(returned.totals(&[10]).tree.cpu_percent, None);
}

#[test]
fn empty_batch_clears_warm_cpu_baselines_before_a_target_returns() {
    let mut collector = Collector::default();
    let now = Instant::now();
    for (offset, expected_cpu) in [(0, None), (1, Some(300.0))] {
        let snapshot = sample(
            &mut collector,
            topology(),
            &[10],
            &[],
            now + Duration::from_secs(offset),
            |_| Some(reading(offset as f64, Some(100), 100)),
        );
        assert_eq!(snapshot.totals(&[10]).tree.cpu_percent, expected_cpu);
    }

    let empty = sample(
        &mut collector,
        Vec::new(),
        &[],
        &[999],
        now + Duration::from_secs(2),
        |_| panic!("empty batches must not read processes"),
    );
    assert!(empty.processes.is_empty());
    assert!(empty.missing_roots.is_empty());
    assert_eq!(empty.totals(&[]), RootMetrics::default());

    for (offset, expected_cpu) in [(3, None), (4, Some(300.0))] {
        let snapshot = sample(
            &mut collector,
            topology(),
            &[10],
            &[],
            now + Duration::from_secs(offset),
            |_| Some(reading(offset as f64, Some(100), 100)),
        );
        assert_eq!(snapshot.totals(&[10]).tree.cpu_percent, expected_cpu);
    }
}

#[test]
fn changed_pid_identity_and_unknown_start_time_do_not_publish_cpu() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    for (offset, started) in [(0, Some(100)), (1, Some(200)), (2, None), (3, None)] {
        let snapshot = sample(
            &mut aggregate,
            vec![process_meta(10, 1, "task", "task")],
            &[10],
            &[],
            now + Duration::from_secs(offset),
            |_| Some(reading(offset as f64 + 1.0, started, 100)),
        );
        assert_eq!(snapshot.totals(&[10]).tree.cpu_percent, None);
        assert_eq!(snapshot.totals(&[10]).tree.rss_bytes, Some(100));
    }
}

#[test]
fn one_missing_root_makes_a_multiple_root_total_incomplete() {
    let mut aggregate = Collector::default();
    let snapshot = sample(
        &mut aggregate,
        topology(),
        &[10, 999],
        &[],
        Instant::now(),
        |_| Some(reading(1.0, Some(100), 100)),
    );
    let totals = snapshot.totals(&[10, 999]);
    assert_eq!(totals.missing_roots, vec![999]);
    assert_eq!(totals.own.process_count, 1);
    assert_eq!(totals.tree.process_count, 3);
    assert_eq!(totals.own.rss_bytes, None);
    assert_eq!(totals.tree.rss_bytes, None);
    assert_eq!(snapshot.totals(&[]), RootMetrics::default());
}

#[test]
fn vanished_child_is_removed_without_retaining_stale_metrics() {
    let mut aggregate = Collector::default();
    let now = Instant::now();
    sample(&mut aggregate, topology(), &[10], &[], now, |_| {
        Some(reading(1.0, Some(100), 100))
    });
    let snapshot = sample(
        &mut aggregate,
        vec![process_meta(10, 1, "root", "root")],
        &[10],
        &[],
        now + Duration::from_secs(1),
        |_| Some(reading(2.0, Some(100), 100)),
    );
    assert_eq!(snapshot.process_ids(&[10]), vec![10]);
    assert_eq!(snapshot.totals(&[10]).tree.cpu_percent, Some(100.0));
    assert_eq!(snapshot.totals(&[10]).tree.rss_bytes, Some(100));
}

#[test]
fn unreadable_sample_resets_cpu_baseline_before_reading_resumes() {
    let mut collector = Collector::default();
    let now = Instant::now();
    for (offset, readable, expected_cpu) in [
        (0, true, None),
        (1, true, Some(100.0)),
        (2, false, None),
        (3, true, None),
        (4, true, Some(100.0)),
    ] {
        let snapshot = sample(
            &mut collector,
            vec![process_meta(10, 1, "task", "task")],
            &[10],
            &[],
            now + Duration::from_secs(offset),
            |_| readable.then(|| reading(offset as f64, Some(100), 1024)),
        );
        assert_eq!(snapshot.totals(&[10]).tree.cpu_percent, expected_cpu);
        assert_eq!(
            snapshot.totals(&[10]).tree.rss_bytes,
            readable.then_some(1024)
        );
    }
}

#[test]
fn zero_elapsed_or_cpu_counter_rollback_is_unknown_but_idle_delta_is_zero() {
    let mut collector = Collector::default();
    let now = Instant::now();
    for (offset, total, expected_cpu) in [
        (0, 2.0, None),
        (0, 3.0, None),
        (1, 1.0, None),
        (2, 1.0, Some(0.0)),
        (3, 2.0, Some(100.0)),
    ] {
        let snapshot = sample(
            &mut collector,
            vec![process_meta(10, 1, "task", "task")],
            &[10],
            &[],
            now + Duration::from_secs(offset),
            |_| Some(reading(total, Some(100), 1024)),
        );
        assert_eq!(snapshot.totals(&[10]).tree.cpu_percent, expected_cpu);
    }
}

#[test]
fn excluded_then_reentered_process_tree_starts_a_fresh_cpu_baseline() {
    let mut collector = Collector::default();
    let now = Instant::now();
    for (offset, excluded, expected_cpu) in [
        (0, false, None),
        (1, false, Some(300.0)),
        (2, true, None),
        (3, false, None),
        (4, false, Some(300.0)),
    ] {
        let snapshot = sample(
            &mut collector,
            topology(),
            &[10],
            if excluded { &[10] } else { &[] },
            now + Duration::from_secs(offset),
            |_| Some(reading(offset as f64, Some(100), 100)),
        );
        let totals = snapshot.totals(&[10]);
        assert_eq!(totals.tree.cpu_percent, expected_cpu);
        assert_eq!(totals.tree.process_count, if excluded { 0 } else { 3 });
        assert_eq!(totals.tree.rss_bytes, (!excluded).then_some(300));
    }
}

#[test]
fn parent_cycles_never_measure_a_process_twice() {
    let mut collector = Collector::default();
    let mut reads = HashMap::<u32, usize>::new();
    let snapshot = sample(
        &mut collector,
        vec![
            process_meta(10, 20, "a", "a"),
            process_meta(20, 10, "b", "b"),
        ],
        &[10, 20],
        &[],
        Instant::now(),
        |pid| {
            *reads.entry(pid).or_default() += 1;
            Some(reading(1.0, Some(100), 100))
        },
    );
    assert!(reads.values().all(|count| *count == 1));
    assert_eq!(snapshot.process_ids(&[10, 20]), vec![10, 20]);
    assert_eq!(snapshot.totals(&[10, 20]).tree.rss_bytes, Some(200));
}

#[test]
fn outer_roots_project_nested_foreground_pids_without_changing_explicit_totals() {
    let mut collector = Collector::default();
    let snapshot = sample(
        &mut collector,
        topology(),
        &[10, 20, 30, 40],
        &[],
        Instant::now(),
        |_| Some(reading(1.0, Some(100), 100)),
    );
    assert_eq!(snapshot.outer_roots(&[30, 10, 20, 10]), vec![10]);
    assert_eq!(snapshot.outer_roots(&[20, 40, 10]), vec![10, 40]);
    assert_eq!(snapshot.totals(&[10, 20, 30]).own.process_count, 3);
    assert_eq!(
        snapshot
            .totals(&snapshot.outer_roots(&[10, 20, 30]))
            .own
            .process_count,
        1
    );
}

#[test]
fn detached_subtree_leaves_its_parent_tree_without_changing_measurements() {
    let mut collector = Collector::default();
    let mut snapshot = sample(
        &mut collector,
        topology(),
        &[10, 20],
        &[],
        Instant::now(),
        |pid| Some(reading(1.0, Some(100), u64::from(pid))),
    );
    assert_eq!(snapshot.process_ids(&[10]), vec![10, 20, 30]);
    snapshot.detach(&[20]);
    assert_eq!(snapshot.process_ids(&[10]), vec![10]);
    assert_eq!(snapshot.process_ids(&[20]), vec![20, 30]);
    assert_eq!(snapshot.processes[&20].ppid, 10);
    assert_eq!(snapshot.totals(&[10]).tree.rss_bytes, Some(10));
    assert_eq!(snapshot.totals(&[20]).tree.rss_bytes, Some(50));
    // The union still counts every process once.
    assert_eq!(snapshot.totals(&[10, 20]).tree.rss_bytes, Some(60));
    assert_eq!(snapshot.totals(&[10, 20]).tree.process_count, 3);
}

#[test]
fn outer_roots_keep_a_higher_pid_parent_and_disjoint_pipeline_members() {
    let mut collector = Collector::default();
    let snapshot = sample(
        &mut collector,
        vec![
            process_meta(10, 40, "child", "child"),
            process_meta(40, 1, "parent", "parent"),
            process_meta(50, 1, "pipeline", "pipeline"),
        ],
        &[10, 40, 50],
        &[],
        Instant::now(),
        |_| Some(reading(1.0, Some(100), 100)),
    );
    assert_eq!(snapshot.outer_roots(&[10, 40, 50]), vec![40, 50]);
}

#[test]
fn outer_roots_preserve_missing_parents_and_observed_children() {
    let mut collector = Collector::default();
    let snapshot = sample(
        &mut collector,
        vec![process_meta(10, 99, "child", "child")],
        &[99, 10],
        &[],
        Instant::now(),
        |_| Some(reading(1.0, Some(100), 100)),
    );
    let outer = snapshot.outer_roots(&[99, 10, 99]);
    assert_eq!(outer, vec![10, 99]);
    assert_eq!(snapshot.totals(&outer).own.process_count, 1);
    assert_eq!(snapshot.totals(&outer).own.rss_bytes, None);
    assert_eq!(snapshot.totals(&outer).missing_roots, vec![99]);
}

#[test]
fn outer_roots_choose_one_cycle_representative_and_remove_its_descendant_roots() {
    let mut collector = Collector::default();
    let snapshot = sample(
        &mut collector,
        vec![
            process_meta(10, 20, "a", "a"),
            process_meta(20, 10, "b", "b"),
            process_meta(5, 20, "child", "child"),
        ],
        &[20, 5, 10],
        &[],
        Instant::now(),
        |_| Some(reading(1.0, Some(100), 100)),
    );
    assert_eq!(snapshot.outer_roots(&[20, 5, 10]), vec![10]);
    assert_eq!(snapshot.outer_roots(&[20, 5]), vec![20]);
    assert_eq!(snapshot.outer_roots(&[]), Vec::<u32>::new());
}
