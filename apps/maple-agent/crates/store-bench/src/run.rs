use crate::{
    Options,
    data::{self, Scale, TaskSpec},
    stats::Samples,
};
use anyhow::{Context, Result, ensure};
use maple_agent::store::bench::{EffectivePragmas, FixtureOwner, PlanCheck, SeedEntry};
use maple_agent::store::keys::{KdfParams, KeyFile, LockedKey, generate_passphrase};
use maple_agent::store::{
    AccountId, AttachmentUploadMetadata, AttachmentUploadStart, HistoryPageRequest,
    NavigationGroupQuery, NavigationSearchQuery, NavigationTaskQuery, NewTask, OpenTaskPageRequest,
    PageTransport, PinFilter, RowRangeRequest, RowReader, Store, StorePaths, StoredAttachment,
    TaskKind, TaskPatch, TaskSection, TaskState,
};
use pi_coding_agent::session::{SESSION_VERSION, SessionHeader};
use pi_coding_agent::store::{JsonlStore, SessionStore};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Plain,
    Keyed,
}

#[derive(Serialize)]
pub struct Run {
    mode: Mode,
    scale: Scale,
    seed: Value,
    engine: maple_agent::store::engine::EngineInfo,
    pragmas: Vec<EffectivePragmas>,
    scenarios: Vec<Samples>,
    pub plans: Vec<PlanCheck>,
    observations: BTreeMap<String, Value>,
}

fn open(paths: StorePaths, mode: Mode) -> Result<Arc<Store>> {
    Ok(Arc::new(match mode {
        Mode::Plain => Store::open(paths)?,
        Mode::Keyed => Store::open_keyed(paths, LockedKey::from_bytes([0x42; 32])?)?,
    }))
}
fn initialize(store: &Store, mode: Mode) -> Result<()> {
    match mode {
        Mode::Plain => store.initialize_plain()?,
        Mode::Keyed => store.initialize_keyed(LockedKey::from_bytes([0x42; 32])?)?,
    };
    Ok(())
}

pub fn execute(mode: Mode, options: &Options) -> Result<Run> {
    eprintln!(
        "Preparing {mode:?} synthetic {}-entry store",
        options.scale.entries
    );
    let temporary = tempfile::tempdir()?;
    let paths = StorePaths::new(temporary.path().join("profile"));
    let _owner = FixtureOwner::acquire(&paths)?;
    let store = open(paths, mode)?;
    let account = store.upsert_account("synthetic@example.invalid", data::BASE_MS)?;
    let specs = data::tasks(options.scale);
    let seed = seed(&store, account, &specs, temporary.path(), None)?;
    let engine = store.engine_info()?;
    let pragmas = store.bench_effective_pragmas()?;
    let writer = pragmas
        .iter()
        .find(|role| role.role == "writer")
        .context("writer settings missing")?;
    ensure!(
        writer.synchronous == 2 && writer.fullfsync == 1 && writer.wal_autocheckpoint == 0,
        "authoritative durability settings changed"
    );
    let mut scenarios = Vec::new();
    let mut observations = BTreeMap::new();
    let iterations = if options.full { 100 } else { 20 };
    let append_count = if options.full { 2000 } else { 200 };
    let hot = &specs[0];
    let mut source = store.bench_source_writer(account, &hot.uuid)?;
    let mut append = Vec::new();
    for seq in hot.entries..hot.entries + append_count {
        let entry = data::entry(hot, seq);
        let started = Instant::now();
        source.append(&entry)?;
        append.push(micros(started));
    }
    scenarios.push(Samples::micros("durable_source_append", append));
    drop(source);
    scenarios.push(measure("raw_connection_open", iterations, || {
        store.bench_raw_connection_open()
    })?);
    let list = NavigationTaskQuery {
        section: TaskSection::State(TaskState::Active),
        project: None,
        pins: PinFilter::Ignore,
        cursor: None,
        limit: 50,
    };
    let request = OpenTaskPageRequest {
        task: hot.uuid.clone(),
        limit_rows: Some(100),
        limit_bytes: None,
    };
    scenarios.push(measure("list_50_warm", iterations, || {
        store
            .list_task_summaries(account, &list, PageTransport::Local)
            .map(|_| ())
    })?);
    scenarios.push(measure("open_100_warm", iterations, || {
        store
            .open_task_page(account, &request, PageTransport::Local)
            .map(|_| ())
    })?);
    let groups = NavigationGroupQuery {
        limit_groups: 20,
        per_project: 10,
        cursor: None,
    };
    let search = NavigationSearchQuery {
        text: "synthetic".into(),
        project: None,
        project_roots: Vec::new(),
        cursor: None,
        limit: 100,
    };
    let ids = specs
        .iter()
        .take(50)
        .map(|task| task.uuid.clone())
        .collect::<Vec<_>>();
    scenarios.push(measure("project_groups_warm", iterations, || {
        store
            .list_project_group_summaries(account, &groups, PageTransport::Local)
            .map(|_| ())
    })?);
    scenarios.push(measure("project_switcher_warm", iterations, || {
        store
            .navigation_projects(account, None, 50, PageTransport::Local)
            .map(|_| ())
    })?);
    scenarios.push(measure("search_100_warm", iterations, || {
        store
            .search_task_summaries(account, &search, PageTransport::Local)
            .map(|_| ())
    })?);
    scenarios.push(measure("get_50_tasks_warm", iterations, || {
        store
            .get_task_summaries(account, &ids, PageTransport::Local)
            .map(|_| ())
    })?);
    scenarios.push(measure("pinned_tasks_warm", iterations, || {
        store
            .pinned_task_summaries(account, None, None, 50, PageTransport::Local)
            .map(|_| ())
    })?);
    scenarios.push(measure("task_facts_warm", iterations, || {
        store.task_facts(account, &hot.uuid).map(|_| ())
    })?);
    let forward = HistoryPageRequest {
        task: hot.uuid.clone(),
        cursor: None,
        limit_rows: Some(200),
        limit_bytes: None,
    };
    scenarios.push(measure("history_forward_200_warm", iterations, || {
        store
            .history_forward_page(account, &forward, PageTransport::Local)
            .map(|_| ())
    })?);
    let tail = store.open_task_page(account, &request, PageTransport::Local)?;
    let mut history = HistoryPageRequest {
        task: hot.uuid.clone(),
        cursor: tail.before,
        limit_rows: Some(200),
        limit_bytes: None,
    };
    let target_depth = 40_000usize.min(hot.entries.saturating_sub(300));
    let mut traversed = 0;
    while traversed < target_depth && history.cursor.is_some() {
        let page = store.history_before_page(account, &history, PageTransport::Local)?;
        traversed += page.rows.len();
        history.cursor = page.next;
    }
    observations.insert("history_depth_rows".into(), json!(traversed));
    if history.cursor.is_some() {
        scenarios.push(measure("history_200_deep_warm", iterations, || {
            store
                .history_before_page(account, &history, PageTransport::Local)
                .map(|_| ())
        })?);
    }
    let mut cold_lists = Vec::new();
    let mut cold_opens = Vec::new();
    let mut cold_history = Vec::new();
    for _ in 0..if options.full { 20 } else { 5 } {
        store.close()?;
        initialize(&store, mode)?;
        let started = Instant::now();
        store.list_task_summaries(account, &list, PageTransport::Local)?;
        cold_lists.push(micros(started));
        store.close()?;
        initialize(&store, mode)?;
        let started = Instant::now();
        store.open_task_page(account, &request, PageTransport::Local)?;
        cold_opens.push(micros(started));
        if history.cursor.is_some() {
            store.close()?;
            initialize(&store, mode)?;
            let started = Instant::now();
            store.history_before_page(account, &history, PageTransport::Local)?;
            cold_history.push(micros(started));
        }
    }
    scenarios.push(Samples::micros("list_50_cold", cold_lists));
    scenarios.push(Samples::micros("open_100_cold", cold_opens));
    if !cold_history.is_empty() {
        scenarios.push(Samples::micros("history_200_deep_cold", cold_history));
    }
    for partial in [false, true] {
        let mut samples = Vec::new();
        let mut counts = Vec::new();
        for _ in 0..5 {
            store.close()?;
            initialize(&store, mode)?;
            let started = Instant::now();
            let loaded = store.bench_context_load(account, &specs[1].uuid, partial)?;
            samples.push(micros(started));
            counts.push(loaded.entries);
        }
        let name = if partial {
            "model_context_boundary_cold"
        } else {
            "model_context_full_cold"
        };
        observations.insert(format!("{name}_entries"), json!(counts));
        scenarios.push(Samples::micros(name, samples));
    }
    let mut source = store.bench_source_writer(account, &hot.uuid)?;
    let held = store.bench_with_held_reader(|| {
        let mut samples = Vec::new();
        for seq in hot.entries + append_count..hot.entries + append_count + 100 {
            let entry = data::entry(hot, seq);
            let started = Instant::now();
            source.append(&entry)?;
            samples.push(micros(started));
        }
        let wal = wal_bytes(&store);
        let started = Instant::now();
        let complete = store.bench_checkpoint(false)?;
        Ok((samples, wal, micros(started), complete))
    })?;
    drop(source);
    scenarios.push(Samples::micros("durable_append_held_reader", held.0));
    scenarios.push(Samples::micros(
        "passive_checkpoint_held_reader",
        vec![held.2],
    ));
    observations.insert("held_reader_wal_bytes".into(), json!(held.1));
    observations.insert("passive_checkpoint_not_busy".into(), json!(held.3));
    let started = Instant::now();
    let truncated = store.bench_checkpoint(true)?;
    scenarios.push(Samples::micros(
        "truncate_checkpoint_after_reader",
        vec![micros(started)],
    ));
    observations.insert("truncate_checkpoint_complete".into(), json!(truncated));
    attachment_scenarios(&store, account, hot, &mut scenarios, &mut observations)?;
    oversized_scenarios(
        &store,
        account,
        temporary.path(),
        &mut scenarios,
        &mut observations,
    )?;
    let mut rebuild = store.bench_begin_rebuild(account, &specs[1].uuid)?;
    let mut chunks = Vec::new();
    let mut writer_times = Vec::new();
    let mut prepare_times = Vec::new();
    loop {
        let chunk = store.bench_rebuild_chunk(&mut rebuild)?;
        let done = chunk.done;
        writer_times.push(chunk.writer_micros);
        prepare_times.push(chunk.prepare_micros);
        chunks.push(chunk);
        if done {
            break;
        }
    }
    scenarios.push(Samples::micros(
        "timeline_rebuild_writer_chunk",
        writer_times,
    ));
    scenarios.push(Samples::micros(
        "timeline_rebuild_prepare_chunk",
        prepare_times,
    ));
    observations.insert(
        "timeline_rebuild_chunks".into(),
        serde_json::to_value(chunks)?,
    );
    let mut cleanup = Vec::new();
    let mut cleanup_chunks = Vec::new();
    loop {
        let chunk = store.bench_collect_old_epoch(account, &specs[1].uuid)?;
        if chunk.rows == 0 {
            break;
        }
        cleanup.push(chunk.writer_micros);
        cleanup_chunks.push(chunk);
    }
    scenarios.push(Samples::micros("old_epoch_collection_chunk", cleanup));
    observations.insert(
        "old_epoch_collection_chunks".into(),
        serde_json::to_value(cleanup_chunks)?,
    );
    jsonl_baseline(
        temporary.path(),
        &specs[1],
        &mut scenarios,
        &mut observations,
    )?;
    let mut plans = store.bench_query_plans(account, &hot.uuid, false)?;
    plans.extend(store.bench_query_plans(account, &hot.uuid, true)?);
    observations.insert(
        "database_bytes".into(),
        json!(fs::metadata(store.paths().database())?.len()),
    );
    observations.insert("wal_bytes".into(), json!(wal_bytes(&store)));
    kdf_scenarios(options.full, &mut scenarios, &mut observations)?;
    if matches!(mode, Mode::Plain) && options.full {
        let password = generate_passphrase()?;
        let bytes = fs::metadata(store.paths().database())?.len();
        let started = Instant::now();
        store.set_store_password(password.password(), &[], KdfParams::floor())?;
        let elapsed = micros(started);
        scenarios.push(Samples::micros(
            "plain_to_keyed_with_floor_kdf",
            vec![elapsed],
        ));
        observations.insert("conversion".into(),json!({"input_bytes":bytes,"elapsed_micros":elapsed,"includes_password_kdf":true,"bytes_per_second":bytes as f64/(elapsed as f64/1_000_000.0)}));
    }
    store.close()?;
    Ok(Run {
        mode,
        scale: options.scale,
        seed,
        engine,
        pragmas,
        scenarios,
        plans,
        observations,
    })
}

fn seed(
    store: &Arc<Store>,
    account: AccountId,
    specs: &[TaskSpec],
    root: &Path,
    project: Option<&str>,
) -> Result<Value> {
    let started = Instant::now();
    let mut payload_bytes = 0usize;
    for task in specs {
        let project = project.map(str::to_owned).unwrap_or_else(|| {
            root.join(format!("project-{:02}", task.index % 40))
                .to_string_lossy()
                .into_owned()
        });
        store.create_task(
            account,
            &NewTask {
                uuid: task.uuid.clone(),
                project_root: project,
                kind: TaskKind::Desktop,
                title: format!("Synthetic task {}", task.index),
                provisional: false,
                model: Some("fixture-model".into()),
                created_ms: task.created_ms,
            },
        )?;
    }
    let mut writers = specs
        .iter()
        .map(|task| store.bench_source_writer(account, &task.uuid))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for order in data::insertion_order(specs).chunks(1000) {
        let mut entries = Vec::with_capacity(order.len());
        for &(task, seq) in order {
            let entry = data::entry(&specs[task], seq);
            payload_bytes += serde_json::to_vec(&entry)?.len();
            entries.push(SeedEntry {
                writer: task,
                entry,
            });
        }
        store.bench_seed_batch(&mut writers, &entries)?;
    }
    drop(writers);
    for task in specs {
        if task.index > 1 {
            store.update_task(
                account,
                &task.uuid,
                &TaskPatch {
                    state: Some(if task.index % 5 == 0 {
                        TaskState::Archived
                    } else if task.index % 5 == 1 {
                        TaskState::Active
                    } else {
                        TaskState::Settled
                    }),
                    ..Default::default()
                },
            )?;
        }
        if task.index % 37 == 0 {
            store.set_task_pinned(account, &task.uuid, true, task.created_ms)?;
        }
    }
    Ok(
        json!({"tasks":specs.len(),"entries":specs.iter().map(|task|task.entries).sum::<usize>(),"payload_bytes":payload_bytes,"elapsed_micros":micros(started),
        "layout":"globally timestamp-interleaved source and projector rows","seed_batch_entries":1000,
        "durability":"FULL with fullfsync; synthetic fixture batches outside timed production source-append measurements"}),
    )
}

fn upload_fixture(
    store: &Store,
    account: AccountId,
    task: &str,
    operation: &str,
    name: &str,
    bytes: &[u8],
) -> Result<StoredAttachment> {
    let metadata = AttachmentUploadMetadata {
        name: name.into(),
        mime_type: "application/octet-stream".into(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    };
    let attachment = match store.begin_attachment_upload(account, task, operation, &metadata)? {
        AttachmentUploadStart::Complete(attachment) => attachment,
        AttachmentUploadStart::Writing(mut writer) => {
            for chunk in bytes.chunks(1024 * 1024) {
                writer.write_chunk(chunk)?;
            }
            writer.finish(store)?
        }
    };
    store.pin_attachments(account, task, std::slice::from_ref(&attachment.id))?;
    Ok(attachment)
}

fn attachment_scenarios(
    store: &Arc<Store>,
    account: AccountId,
    task: &TaskSpec,
    scenarios: &mut Vec<Samples>,
    observations: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let bytes = vec![0u8; 10 * 1024 * 1024];
    let attachment = upload_fixture(
        store,
        account,
        &task.uuid,
        "benchmark-read-file",
        "synthetic.bin",
        &bytes,
    )?;
    let read = || -> Result<usize> {
        let mut offset = 0;
        while offset < bytes.len() {
            let range = store.read_attachment(
                account,
                &task.uuid,
                &attachment.id,
                offset as u64,
                1024 * 1024,
            )?;
            ensure!(!range.data.is_empty(), "empty attachment range");
            offset += range.data.len();
        }
        Ok(offset)
    };
    let started = Instant::now();
    ensure!(read()? == bytes.len());
    scenarios.push(Samples::micros(
        "attachment_10mib_first_read",
        vec![micros(started)],
    ));
    scenarios.push(measure("attachment_10mib_warm", 5, || read().map(|_| ()))?);
    observations.insert("attachment_bytes".into(), json!(bytes.len()));
    observations.insert(
        "attachment_storage".into(),
        json!("plaintext session-owned file; database metadata only"),
    );
    Ok(())
}

fn oversized_scenarios(
    store: &Arc<Store>,
    account: AccountId,
    root: &Path,
    scenarios: &mut Vec<Samples>,
    observations: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let task = TaskSpec {
        index: 999_999,
        uuid: "018ddbcc-b80b-7c20-8123-ffffffffffff".into(),
        entries: 0,
        created_ms: data::BASE_MS,
        gap_ms: 10,
    };
    store.create_task(
        account,
        &NewTask {
            uuid: task.uuid.clone(),
            project_root: root.join("oversized").to_string_lossy().into_owned(),
            kind: TaskKind::Desktop,
            title: "Synthetic oversized write".into(),
            provisional: false,
            model: Some("fixture-model".into()),
            created_ms: task.created_ms,
        },
    )?;
    // Each task owns a separate file. Deletion measures file unlink and durable
    // metadata removal, not a SQL payload rewrite or a shared blob reference.
    let bytes = vec![0x55; 10 * 1024 * 1024];
    let started = Instant::now();
    upload_fixture(
        store,
        account,
        &task.uuid,
        "benchmark-deleted-file",
        "synthetic-deleted.bin",
        &bytes,
    )?;
    scenarios.push(Samples::micros(
        "durable_file_upload_10mib",
        vec![micros(started)],
    ));
    let entry = data::oversized_entry(&task, 0, 20 * 1024 * 1024);
    let mut source = store.bench_source_writer(account, &task.uuid)?;
    let started = Instant::now();
    source.append(&entry)?;
    scenarios.push(Samples::micros(
        "durable_20mib_write",
        vec![micros(started)],
    ));
    drop(source);
    let page = store.open_task_page(
        account,
        &OpenTaskPageRequest {
            task: task.uuid.clone(),
            limit_rows: Some(100),
            limit_bytes: None,
        },
        PageTransport::Local,
    )?;
    let row = page.rows.last().context("oversized tool row missing")?;
    let reader = RowReader::new(Arc::clone(store));
    let request = RowRangeRequest {
        task: task.uuid.clone(),
        rows_epoch: row.rows_epoch,
        row_seq: row.row_seq,
        update_seq: Some(row.update_seq),
        offset: 0,
        len: 1024 * 1024,
    };
    let started = Instant::now();
    let range = reader.read_row(account, &request, PageTransport::Local)?;
    scenarios.push(Samples::micros(
        "oversized_row_first_range",
        vec![micros(started)],
    ));
    scenarios.push(measure("oversized_row_cached_range", 5, || {
        reader
            .read_row(account, &request, PageTransport::Local)
            .map(|_| ())
    })?);
    observations.insert("oversized_row_total_bytes".into(), json!(range.total));
    reader.clear()?;
    store.mark_task_deleted(account, &task.uuid, data::BASE_MS + 1)?;
    let mut collection = Vec::new();
    let mut durations = Vec::new();
    loop {
        let wal_before_bytes = wal_bytes(store);
        let started = Instant::now();
        let chunk = store.collect_task_step(account, &task.uuid)?;
        durations.push(micros(started));
        let wal_after_bytes = wal_bytes(store);
        let wal_file_delta_bytes =
            i64::try_from(i128::from(wal_after_bytes) - i128::from(wal_before_bytes))?;
        let done = chunk.done;
        collection.push(json!({"stage":format!("{:?}",chunk.stage),"rows":chunk.rows,"bytes":chunk.bytes,"oversized_row":chunk.oversized_row,"elapsed_micros":chunk.elapsed_micros,"done":done,
            "wal_before_bytes":wal_before_bytes,"wal_after_bytes":wal_after_bytes,"wal_file_delta_bytes":wal_file_delta_bytes}));
        if done {
            break;
        }
    }
    scenarios.push(Samples::micros(
        "oversized_task_collection_chunk",
        durations,
    ));
    observations.insert("oversized_collection_chunks".into(), json!(collection));
    Ok(())
}

fn jsonl_baseline(
    root: &Path,
    task: &TaskSpec,
    scenarios: &mut Vec<Samples>,
    observations: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let path = root.join("synthetic-only.jsonl");
    let header = SessionHeader {
        version: SESSION_VERSION,
        id: task.uuid.clone(),
        timestamp: task.created_ms,
        cwd: root.to_string_lossy().into_owned(),
        parent_session: None,
    };
    let entries = (0..task.entries)
        .map(|seq| data::entry(task, seq))
        .collect::<Vec<_>>();
    let mut store = JsonlStore::new(&path);
    store.write_all(&header, &entries)?;
    scenarios.push(measure("jsonl_full_load_os_cache_warm", 5, || {
        JsonlStore::load(&path).map(|_| ())
    })?);
    let mut append = Vec::new();
    for seq in task.entries..task.entries + 100 {
        let entry = data::entry(task, seq);
        let started = Instant::now();
        store.append(&entry)?;
        append.push(micros(started));
    }
    scenarios.push(Samples::micros(
        "jsonl_append_unsynced_historical_baseline",
        append,
    ));
    observations.insert(
        "jsonl_baseline".into(),
        json!({"synthetic_only":true,"append_fsync":false,"bytes":fs::metadata(path)?.len()}),
    );
    Ok(())
}

fn kdf_scenarios(
    full: bool,
    scenarios: &mut Vec<Samples>,
    observations: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let params = if full {
        if let Some(ram) = crate::physical_ram() {
            let started = Instant::now();
            let params = KdfParams::calibrate(ram)?;
            observations.insert(
                "argon2_calibration".into(),
                json!({"ram_bytes":ram,"params":params,"elapsed_micros":micros(started)}),
            );
            params
        } else {
            KdfParams::floor()
        }
    } else {
        KdfParams::floor()
    };
    let generated = generate_passphrase()?;
    let key = LockedKey::from_bytes([0x24; 32])?;
    let slot = KeyFile::create_generated(&generated, &key, [0x13; 16], params)?;
    let started = Instant::now();
    let _key = slot.unlock(generated.password())?;
    scenarios.push(Samples::micros("argon2_unlock", vec![micros(started)]));
    observations.insert("argon2_params".into(), serde_json::to_value(params)?);
    Ok(())
}

pub fn seed_profile(options: &Options) -> Result<()> {
    let xdg = PathBuf::from(
        std::env::var_os("XDG_DATA_HOME")
            .context("seed requires XDG_DATA_HOME from the selected managed workspace")?,
    );
    ensure!(xdg.is_absolute(), "XDG_DATA_HOME must be absolute");
    let paths = StorePaths::new(xdg.join(&options.namespace));
    let _owner = FixtureOwner::acquire(&paths)?;
    ensure!(
        paths.database().exists(),
        "sign into this profile's new store before seeding"
    );
    let store = Arc::new(Store::open(paths)?);
    let account = store
        .bench_signed_in_account()?
        .context("the selected plain store has no signed-in account")?;
    let root = store.paths().root().to_path_buf();
    let specs = data::tasks(options.scale);
    let metadata = seed(&store, account, &specs, &root, options.project.as_deref())?;
    store.close()?;
    println!(
        "{}",
        json!({"database":store.paths().database(),"bytes":fs::metadata(store.paths().database())?.len(),"seed":metadata})
    );
    Ok(())
}

fn measure<E: Into<anyhow::Error>>(
    name: &str,
    count: usize,
    mut action: impl FnMut() -> std::result::Result<(), E>,
) -> Result<Samples> {
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let started = Instant::now();
        action().map_err(Into::into)?;
        values.push(micros(started));
    }
    Ok(Samples::micros(name, values))
}
fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}
fn wal_bytes(store: &Store) -> u64 {
    fs::metadata(store.paths().db().join("maple.db-wal")).map_or(0, |metadata| metadata.len())
}
