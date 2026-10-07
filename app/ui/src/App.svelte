<script lang="ts">
 import { onMount } from 'svelte';
 import { listen, type UnlistenFn } from '@tauri-apps/api/event';
 import { api, clock, meter, badge, busy, type Config, type CoreEvent, type Devices, type MeetingEntry, type MeetingView, type RecordingState } from './api';
 import Player from './Player.svelte';
 import { open } from '@tauri-apps/plugin-dialog';
 let config = $state<Config | null>(null);
 let draft = $state<Config | null>(null);
 let devices = $state<Devices>({inputs: [], outputs: []});
 let entries = $state<MeetingEntry[]>([]);
 let recordingState = $state<RecordingState>('ready');
 let elapsed = $state(0);
 let mic = $state(0); let system = $state(0);
 let screen = $state<'home' | 'meeting' | 'settings'>('home');
 let closing = $state(false); let ready = $state(false);
 let monitoring = $state(false); let userPaused = $state(false);
 let documentVisible = $state(true); let windowVisible = $state(false);
 let toast = $state(''); let toastTimer: ReturnType<typeof setTimeout>;
 let requestedMonitoring: boolean | null = null;
 let listening = $derived(screen === 'home' && documentVisible && windowVisible && !userPaused && !closing);
 let metersPaused = $derived(recordingState !== 'recording' && (!listening || !monitoring));
 $effect(() => {
   const wanted = ready && listening;
   if (!ready) return;
   const timer = setTimeout(() => {
     if (requestedMonitoring !== wanted) {
       requestedMonitoring = wanted;
       void api.monitoring(wanted).catch(error => { requestedMonitoring = null; notice = String(error); });
     }
   }, 300);
   return () => clearTimeout(timer);
 });
 let selected = $state<string | null>(null);
 let view = $state<MeetingView | null>(null);
 let title = $state(''); let notice = $state('');
 let progress = $state<Record<string, {stage: string; fraction: number}>>({});
 let player = $state<Player>();
 let loadVersion = 0;
 let bootstrapping = true;
 const pendingEvents: CoreEvent[] = [];
 let refreshing = false;
 let refreshAgain = false;
 let libraryBusy = $state<Record<string, boolean>>({});
 let selectedJob = $derived(entries.find(e => e.meeting.dir === selected)?.job ?? null);
 let lastJobFailure = $derived(typeof selectedJob === 'object' && selectedJob !== null ? selectedJob.failed : null);
 let canRecord = $derived(ready && !closing && (recordingState === 'ready' || recordingState === 'recording'));
 async function action(work: () => Promise<unknown>) { try { await work(); } catch (error) { notice = String(error); } }
 async function refresh() {
   if (refreshing) { refreshAgain = true; return; }
   refreshing = true;
   try { do { refreshAgain = false; entries = await api.meetings(); } while (refreshAgain); }
   finally { refreshing = false; }
 }
 async function openMeeting(dir: string) {
   const version = ++loadVersion; selected = dir; screen = 'meeting'; view = null;
   try { const result = await api.meeting(dir); if (version === loadVersion && selected === dir) { view = result; title = result.meeting.title; } }
   catch (error) { if (version === loadVersion) notice = String(error); }
 }
 async function rename() {
   if (!selected || !view || title.trim() === view.meeting.title) return;
   const dir = selected;
   await action(async () => { const renamed = await api.rename(dir, title); await refresh(); await openMeeting(renamed); });
 }
 type Folder = 'meetings_dir' | 'transcripts_dir' | 'archive_dir';
 const folders: {key: Folder; label: string}[] = [{key:'meetings_dir',label:'Meetings folder'}, {key:'transcripts_dir',label:'Transcripts folder'}, {key:'archive_dir',label:'Archive folder'}];
 const models = [{value:'scribe_v2',label:'scribe_v2 (recommended, best accuracy for recorded calls)'}, {value:'scribe_v2_medical',label:'scribe_v2_medical (tuned for clinical terminology)'}, {value:'scribe_v1',label:'scribe_v1 (previous generation, legacy)'}];
 const languages = [{value:'it',label:'Italian'}, {value:'en',label:'English'}, {value:'th',label:'Thai'}, {value:'es',label:'Spanish'}, {value:'fr',label:'French'}, {value:'de',label:'German'}, {value:'pt',label:'Portuguese'}, {value:'zh',label:'Chinese'}, {value:'ja',label:'Japanese'}, {value:'hi',label:'Hindi'}, {value:'ar',label:'Arabic'}, {value:'ru',label:'Russian'}];
 let defaults = $state<Record<Folder,string>>({meetings_dir:'',transcripts_dir:'',archive_dir:''});
 let editing = $state<keyof Config | null>(null);
 const dirty = new Set<keyof Config>();
 let saved = $state(false); let savedTimer: ReturnType<typeof setTimeout>;
 let keyStatus = $state<'idle' | 'testing' | 'valid' | 'error'>('idle');
 let keyError = $state(''); let keyVersion = 0;
 let saves = Promise.resolve();
 let saving: {field: keyof Config; value: Config[keyof Config]; resolve: () => void; reject: (error: unknown) => void} | null = null;
 function applyConfig(next: Config) {
   const keyChanged = config?.elevenlabs_api_key !== next.elevenlabs_api_key;
   config = next;
   if (!draft) draft = {...next};
   else for (const field of Object.keys(next) as (keyof Config)[]) {
     if (field !== editing && !dirty.has(field)) Object.assign(draft, {[field]: next[field]});
   }
   if (keyChanged && !dirty.has('elevenlabs_api_key')) { keyVersion++; keyStatus = 'idle'; keyError = ''; }
   if (saving && next[saving.field] === saving.value) saving.resolve();
 }
 function changed(field: keyof Config) {
   dirty.add(field);
   if (field === 'elevenlabs_api_key') { keyVersion++; keyStatus = 'idle'; keyError = ''; }
 }
 function saveField<K extends keyof Config>(field: K, value: Config[K]): Promise<void> {
   // Serialize field patches, using the latest persisted config rather than a stale draft.
   const work = saves.then(async () => {
     const latest = await api.getConfig();
     if (latest[field] !== value) {
       await new Promise<void>((resolve, reject) => {
         const timer = setTimeout(() => reject(new Error('Settings save was not acknowledged')), 5000);
         saving = {field, value, resolve: () => {clearTimeout(timer); resolve();}, reject: error => {clearTimeout(timer); reject(error);}};
         void api.config({...latest, [field]:value}).catch(error => saving?.reject(error));
       }).finally(() => saving = null);
       saved = true; clearTimeout(savedTimer); savedTimer = setTimeout(() => saved = false, 1500);
     }
     if (draft?.[field] === value || (typeof draft?.[field] === 'string' && (draft[field] as string).trim() === (value ?? ''))) dirty.delete(field);
     applyConfig(await api.getConfig());
   });
   saves = work.catch(error => { notice = String(error); });
   return work;
 }
 function blurField(field: 'your_name' | Folder) {
   editing = null;
   if (draft && dirty.has(field)) void action(() => saveField(field, draft![field]?.trim() || null));
   else if (config) applyConfig(config);
 }
 async function pickFolder(field: Folder) {
   if (!defaults[field]) defaults = await api.folderDefaults();
   const selected = await open({directory:true, multiple:false, defaultPath:draft?.[field]?.trim() || defaults[field]});
   if (selected && draft) { draft[field] = selected; changed(field); await saveField(field, selected); }
 }
 async function checkKey(save: boolean) {
   if (!draft || keyStatus === 'testing') return;
   const value = draft.elevenlabs_api_key?.trim() || null;
   const version = ++keyVersion; keyStatus = 'testing'; keyError = '';
   try {
     await api.testKey(save ? value ?? '' : null);
     if (version !== keyVersion) return;
     if (save) { await saveField('elevenlabs_api_key', value); }
     // An edit during the request invalidates its result even if it later matches again.
     if (version === keyVersion && (draft.elevenlabs_api_key?.trim() || null) === value) keyStatus = 'valid';
   } catch (error) {
     if (version === keyVersion) {
       keyStatus = !save && String(error) === 'No API key configured' ? 'idle' : 'error';
       keyError = String(error);
     }
   }
 }
 async function removeKey() {
   if (!draft || !confirm('Remove the saved ElevenLabs API key? An environment key will still be used if set.')) return;
   draft.elevenlabs_api_key = null; changed('elevenlabs_api_key');
   await saveField('elevenlabs_api_key', null);
   keyStatus = 'idle'; keyError = '';
 }
 function settings() {
   if (screen === 'settings') return;
   screen = 'settings'; if (config) draft = {...config}; dirty.clear(); editing = null;
   void action(async () => { defaults = await api.folderDefaults(); });
   void checkKey(false);
 }
 // Unmount the player first so WebView2 releases the audio files before they move.
 async function library(kind: 'delete' | 'archive') {
   const dir = selected; if (!dir || !view) return;
   const name = view.meeting.title;
   const hasTranscript = view.meeting.utterances.length > 0;
   const question = kind === 'delete'
     ? `Delete the audio of "${name}"? The folder goes to the Recycle Bin.${hasTranscript ? ' The transcript is kept in the transcripts folder.' : ''}`
     : `Archive "${name}" as a zip in the archive folder? The local copy is removed after the zip is verified.`;
   if (!confirm(question)) return;
   view = null;
   try { await (kind === 'delete' ? api.deleteAudio(dir) : api.archive(dir)); }
   catch (error) { notice = String(error); await openMeeting(dir); }
 }
 async function handle(event: CoreEvent) {
   switch(event.type) {
     case 'levels': mic = event.mic; system = event.system; break;
     case 'monitoring_changed': monitoring = event.active; if (!event.active) {mic = 0; system = 0;} break;
     case 'notice': toast = event.message; clearTimeout(toastTimer); toastTimer = setTimeout(() => toast = '', 5000); break;
     case 'recording_state_changed': recordingState = event.state; elapsed = event.elapsed_ms; break;
     case 'devices_changed': devices = event.devices; break;
     case 'config_changed': applyConfig(event.config); break;
     case 'job_failed': notice = event.error; await refresh(); break;
     case 'job_progress': progress[event.meeting] = { stage: event.stage, fraction: event.progress }; break;
     case 'meetings_changed': case 'job_queued': case 'job_cancelled': await refresh(); break;
     case 'job_done': await refresh(); if (selected === event.meeting) await openMeeting(selected); break;
     case 'library_busy': libraryBusy[event.meeting] = event.busy; break;
     case 'library_done':
       delete libraryBusy[event.meeting];
       notice = event.action === 'archive' ? `Archived to ${event.output}` : event.output ? `Audio deleted. Transcript saved to ${event.output}` : 'Audio deleted.';
       await refresh(); if (selected === event.meeting) { screen = 'home'; selected = null; view = null; } break;
     case 'error': notice = event.message; if (event.message.toLowerCase().includes('config')) saving?.reject(event.message); if (selected && screen === 'meeting' && !view && !libraryBusy[selected]) await openMeeting(selected); break;
     case 'shutdown_complete': closing = true; break;
   }
 }
 onMount(() => {
   let disposed = false; const unlisten: UnlistenFn[] = [];
   const visibility = () => documentVisible = document.visibilityState === 'visible';
   visibility(); document.addEventListener('visibilitychange', visibility);
   // Subscribe before the snapshot so startup and reconnect cannot lose recordingState.
   void (async () => {
     const events = await listen<CoreEvent>('core-event', e => { if (bootstrapping) pendingEvents.push(e.payload); else void action(() => handle(e.payload)); });
     if (disposed) { events(); return; } unlisten.push(events);
     const close = await listen('closing', () => { closing = true; });
     if (disposed) { close(); return; } unlisten.push(close);
     const visible = await listen<boolean>('window-visibility', e => windowVisible = e.payload);
     if (disposed) { visible(); return; } unlisten.push(visible);
     windowVisible = await api.windowVisible();
     const snapshot = await api.snapshot(); if (disposed) return;
     config = snapshot.config; draft = {...snapshot.config}; devices = snapshot.devices;
     monitoring = snapshot.monitoring;
     entries = snapshot.meetings; recordingState = snapshot.state; elapsed = snapshot.elapsed_ms; closing = snapshot.closing; ready = true;
     bootstrapping = false;
     for (const event of pendingEvents.splice(0)) await handle(event);
     await api.devices();
   })().catch(error => { bootstrapping = false; notice = String(error); });
   return () => { disposed = true; document.removeEventListener('visibilitychange', visibility); clearTimeout(toastTimer); clearTimeout(savedTimer); keyVersion++; for (const stop of unlisten) stop(); };
 });
</script>

<svelte:head><title>Sottovoce</title></svelte:head>
<div class="shell">
 <aside>
  <button class="brand" title="Home" onclick={() => screen = 'home'}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m3 10 9-7 9 7v11h-6v-7H9v7H3Z" /></svg>Sottovoce</button>
  <div class="rec-panel" class:meters-paused={metersPaused}>
   <button class="record" disabled={!canRecord} onclick={() => action(recordingState === 'recording' ? api.stop : api.start)}>{recordingState === 'recording' ? '■ Stop' : recordingState === 'starting' ? 'Starting…' : recordingState === 'finalizing' ? 'Finalizing…' : '● Rec'}</button>
   <span class="timer">{clock(elapsed)}</span>
   <div class="mini"><span>Mic</span><div class="meter"><i style:width={`${meter(mic)}%`}></i></div></div>
   <div class="mini"><span>System</span><div class="meter"><i style:width={`${meter(system)}%`}></i></div></div>
  </div>
  <div class="list-heading">Meetings <button class="quiet" onclick={() => action(refresh)}>Refresh</button></div>
  <nav aria-label="Meetings">
   {#each entries as entry (entry.meeting.dir)}
    <button class:selected={selected === entry.meeting.dir && screen === 'meeting'} onclick={() => openMeeting(entry.meeting.dir)}>
     <strong>{entry.meeting.title}</strong><span>{new Date(entry.meeting.started_at_unix_ms).toLocaleString()} · {clock(entry.meeting.duration_ms)}</span>
     {#if entry.job && entry.job !== 'done'}<small class="badge">{badge(entry.job)}</small>{/if}
    </button>
   {/each}
   {#if entries.length === 0}<p class="empty">Your recordings will appear here.</p>{/if}
  </nav>
  <button class="settings-link" onclick={settings}>Settings</button>
 </aside>
  <main>
  {#if toast}<div class="device-toast" role="status">{toast}</div>{/if}
  {#if notice}<div class="notice" role="alert"><span>{notice}</span><button onclick={() => notice = ''}>Dismiss</button></div>{/if}
  {#if !ready}<p>Connecting to recorder…</p>
  {:else if screen === 'home'}
   <header><h1>{recordingState === 'recording' ? 'Recording' : 'Ready to record… quietly.'}</h1><p>Your microphone and system audio are saved as separate tracks.</p></header>
   <section class="card capture" class:meters-paused={metersPaused}>
    <div class="listening-controls"><button onclick={() => userPaused = !userPaused}>{userPaused ? 'Resume listening' : 'Pause listening'}</button>{#if metersPaused}<span>Listening paused</span>{/if}</div>
    <div class="device-row"><label for="mic">Microphone</label><select id="mic" value={config?.mic_device ?? ''} disabled={recordingState !== 'ready'} onchange={e => action(() => api.mic(e.currentTarget.value || null))}>
     <option value="">Windows default</option>
     {#if config?.mic_device && !devices.inputs.some(d => d.id === config?.mic_device)}<option value={config.mic_device}>Unavailable — using default</option>{/if}
     {#each devices.inputs as device}<option value={device.id}>{device.name}{device.is_default ? ' (default)' : ''}</option>{/each}
    </select></div>
    <div class="big-meter"><div class="meter"><i style:width={`${meter(mic)}%`}></i></div><span>{mic >= 0.001 ? Math.round(20 * Math.log10(mic)) : '−∞'} dBFS</span></div>
    <div class="device-row"><label for="system">System audio</label><select id="system" value={config?.output_device ?? ''} disabled={recordingState !== 'ready'} onchange={e => action(() => api.output(e.currentTarget.value || null))}>
     <option value="">Windows default</option>
     {#if config?.output_device && !devices.outputs.some(d => d.id === config?.output_device)}<option value={config.output_device}>Unavailable — using default</option>{/if}
     {#each devices.outputs as device}<option value={device.id}>{device.name}{device.is_default ? ' (default)' : ''}</option>{/each}
    </select></div>
    <div class="big-meter"><div class="meter"><i style:width={`${meter(system)}%`}></i></div><span>{system >= 0.001 ? Math.round(20 * Math.log10(system)) : '−∞'} dBFS</span></div>
    <button class="quiet" onclick={() => action(api.devices)}>Refresh devices</button>
    <div class="capture-action"><div class="large-timer">{clock(elapsed)}</div><button class="record large" disabled={!canRecord} onclick={() => action(recordingState === 'recording' ? api.stop : api.start)}>{recordingState === 'recording' ? 'Stop recording' : recordingState === 'starting' ? 'Starting…' : recordingState === 'finalizing' ? 'Finalizing…' : 'Start recording'}</button></div>
   </section>
   <p class="hint">Check both levels before starting. Transcription runs separately, on demand.</p>
  {:else if screen === 'meeting'}
   {#if view && selected}
    <header><label class="sr-only" for="title">Meeting title</label><input id="title" class="title" bind:value={title} onblur={rename} onkeydown={e => { if (e.key === 'Enter') e.currentTarget.blur(); }} disabled={busy(selectedJob)} /><p>{new Date(view.meeting.started_at_unix_ms).toLocaleString()} · {clock(view.meeting.duration_ms)}</p></header>
    {#key selected}<Player bind:this={player} mic={view.mic} system={view.system} durationMs={view.meeting.duration_ms} onerror={error => notice = error} />{/key}
    {#if lastJobFailure}<p class="notice">{lastJobFailure}</p>{/if}
    <div class="actions">
     <button disabled={busy(selectedJob) || closing} onclick={() => selected && action(() => api.transcribe(selected!))}>{busy(selectedJob) ? badge(selectedJob) : entries.find(e => e.meeting.dir === selected)?.meeting.transcribed ? 'Transcribe again' : 'Transcribe'}</button>
     {#if busy(selectedJob)}<button onclick={() => selected && action(() => api.cancel(selected!))}>Cancel job</button>{/if}
     <button disabled={busy(selectedJob) || closing || libraryBusy[selected]} onclick={() => action(() => library('delete'))}>Delete audio</button>
     <button disabled={busy(selectedJob) || closing || libraryBusy[selected]} onclick={() => action(() => library('archive'))}>Archive</button>
    </div>
    {#if busy(selectedJob) && progress[selected]}<div class="job-status"><span>{progress[selected].stage}</span><progress max="1" value={progress[selected].fraction}></progress></div>{/if}
    <section class="transcript" aria-label="Transcript">
     {#each view.meeting.utterances as line}
      <button class="line" onclick={() => player?.seek(line.start_ms / 1000)}><span class="timestamp">{clock(line.start_ms)}</span><strong>{view.meeting.speakers.find(s => s.id === line.speaker)?.name ?? line.speaker}</strong><span>{line.text}</span></button>
     {/each}
     {#if view.meeting.utterances.length === 0}<p class="empty">No transcript yet. Press Transcribe when you want to process this recording.</p>{/if}
    </section>
   {:else}<p>Loading meeting…</p>{/if}
  {:else if draft}
   <header><h1>Settings</h1><p>Changes are saved automatically. Edits to config.toml show up here too.</p><span class="saved" role="status">{saved ? 'Saved' : ''}</span></header>
   <div class="card settings">
    <div class="setting"><label for="api-key">ElevenLabs API key</label><div class="field-row"><input id="api-key" type="password" autocomplete="off" bind:value={draft.elevenlabs_api_key} onfocus={() => editing = 'elevenlabs_api_key'} onblur={() => editing = null} oninput={() => changed('elevenlabs_api_key')} placeholder="Uses ELEVENLABS_API_KEY when empty" /><button disabled={keyStatus === 'testing' || closing} onclick={() => checkKey(true)}>{#if keyStatus === 'testing'}<span class="spinner" aria-label="Testing"></span>{/if}Test &amp; save</button>{#if keyStatus === 'valid'}<span class="key-valid" title="API key verified" role="status">✓</span>{/if}</div>
     {#if config?.elevenlabs_api_key}<button class="quiet remove-key" disabled={keyStatus === 'testing'} onclick={() => action(removeKey)}>Remove</button>{/if}
     {#if keyStatus === 'error'}<small class="key-error" role="alert">{keyError}</small>{/if}
    </div>
    <label>Your name<input bind:value={draft.your_name} placeholder="You" onfocus={() => editing = 'your_name'} oninput={() => changed('your_name')} onblur={() => blurField('your_name')} onkeydown={e => {if(e.key === 'Enter') e.currentTarget.blur();}} /></label>
    {#each folders as folder}
     <div class="setting"><label for={folder.key}>{folder.label}</label><div class="field-row"><input id={folder.key} bind:value={draft[folder.key]} placeholder={defaults[folder.key]} onfocus={() => editing = folder.key} oninput={() => changed(folder.key)} onblur={() => blurField(folder.key)} onkeydown={e => {if(e.key === 'Enter') e.currentTarget.blur();}} /><button class="folder-button" title={`Choose ${folder.label.toLowerCase()}`} aria-label={`Choose ${folder.label.toLowerCase()}`} onclick={() => action(() => pickFolder(folder.key))}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7V4h7l2 3h9v3M3 7h8l2 3h9l-3 10H3Z" /></svg></button></div></div>
    {/each}
    <label>STT model<select value={draft.stt_model || 'scribe_v2'} onchange={e => {if(draft){draft.stt_model = e.currentTarget.value; changed('stt_model'); void action(() => saveField('stt_model', draft!.stt_model));}}}>
     {#each models as model}<option value={model.value}>{model.label}</option>{/each}
     {#if draft.stt_model && !models.some(m => m.value === draft?.stt_model)}<option value={draft.stt_model}>{draft.stt_model} (custom)</option>{/if}
    </select></label>
    <label>Language<select value={!draft.language || draft.language === 'auto' ? '' : draft.language} onchange={e => {if(draft){draft.language = e.currentTarget.value || null; changed('language'); void action(() => saveField('language', draft!.language));}}}>
     <option value="">Auto-detect (recommended)</option>
     {#each languages as language}<option value={language.value}>{language.label}</option>{/each}
     {#if draft.language && draft.language !== 'auto' && !languages.some(l => l.value === draft?.language)}<option value={draft.language}>{draft.language} (custom)</option>{/if}
    </select></label>
    <label class="checkbox"><input type="checkbox" checked={draft.auto_transcribe ?? false} onchange={e => {if(draft){draft.auto_transcribe = e.currentTarget.checked; changed('auto_transcribe'); void action(() => saveField('auto_transcribe', draft!.auto_transcribe));}}} />Transcribe automatically after Stop (uses ElevenLabs credit)</label>
    <label class="checkbox" title="Runs after recording when you press Transcribe. Nemotron finds voices on each side, labelled Remote 1, Remote 2…"><input type="checkbox" checked={draft.diarize ?? true} onchange={e => {if(draft){draft.diarize = e.currentTarget.checked; changed('diarize'); void action(() => saveField('diarize', draft!.diarize));}}} />Detect multiple voices</label>
   </div>
  {/if}
 </main>
</div>
{#if closing}<div class="closing" role="status"><div class="card"><h2>Finalizing…</h2><p>Saving audio and cancelling transcription before closing.</p></div></div>{/if}
