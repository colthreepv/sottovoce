<script lang="ts">
 import { onMount } from 'svelte';
 import { listen, type UnlistenFn } from '@tauri-apps/api/event';
 import { api, clock, meter, badge, busy, type Config, type CoreEvent, type Devices, type MeetingEntry, type MeetingView, type RecordingState } from './api';
 import Player from './Player.svelte';
 let config = $state<Config | null>(null);
 let draft = $state<Config | null>(null);
 let devices = $state<Devices>({inputs: [], outputs: []});
 let entries = $state<MeetingEntry[]>([]);
 let recordingState = $state<RecordingState>('ready');
 let elapsed = $state(0);
 let mic = $state(0); let system = $state(0);
 let screen = $state<'home' | 'meeting' | 'settings'>('home');
 let selected = $state<string | null>(null);
 let view = $state<MeetingView | null>(null);
 let title = $state(''); let notice = $state(''); let closing = $state(false); let ready = $state(false);
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
 function settings() { screen = 'settings'; if (config) draft = {...config}; }
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
     case 'recording_state_changed': recordingState = event.state; elapsed = event.elapsed_ms; break;
     case 'devices_changed': devices = event.devices; break;
     case 'config_changed': config = event.config; draft = {...event.config}; break;
     case 'job_failed': notice = event.error; await refresh(); break;
     case 'job_progress': progress[event.meeting] = { stage: event.stage, fraction: event.progress }; break;
     case 'meetings_changed': case 'job_queued': case 'job_cancelled': await refresh(); break;
     case 'job_done': await refresh(); if (selected === event.meeting) await openMeeting(selected); break;
     case 'library_busy': libraryBusy[event.meeting] = event.busy; break;
     case 'library_done':
       delete libraryBusy[event.meeting];
       notice = event.action === 'archive' ? `Archived to ${event.output}` : event.output ? `Audio deleted. Transcript saved to ${event.output}` : 'Audio deleted.';
       await refresh(); if (selected === event.meeting) { screen = 'home'; selected = null; view = null; } break;
     case 'error': notice = event.message; if (selected && screen === 'meeting' && !view && !libraryBusy[selected]) await openMeeting(selected); break;
     case 'shutdown_complete': closing = true; break;
   }
 }
 onMount(() => {
   let disposed = false; const unlisten: UnlistenFn[] = [];
   // Subscribe before the snapshot so startup and reconnect cannot lose recordingState.
   void (async () => {
     const events = await listen<CoreEvent>('core-event', e => { if (bootstrapping) pendingEvents.push(e.payload); else void action(() => handle(e.payload)); });
     if (disposed) { events(); return; } unlisten.push(events);
     const close = await listen('closing', () => { closing = true; });
     if (disposed) { close(); return; } unlisten.push(close);
     const snapshot = await api.snapshot(); if (disposed) return;
     config = snapshot.config; draft = {...snapshot.config}; devices = snapshot.devices;
     entries = snapshot.meetings; recordingState = snapshot.state; elapsed = snapshot.elapsed_ms; closing = snapshot.closing; ready = true;
     bootstrapping = false;
     for (const event of pendingEvents.splice(0)) await handle(event);
     await api.devices();
   })().catch(error => { bootstrapping = false; notice = String(error); });
   return () => { disposed = true; for (const stop of unlisten) stop(); };
 });
</script>

<svelte:head><title>Sottovoce</title></svelte:head>
<div class="shell">
 <aside>
  <button class="brand" onclick={() => screen = 'home'}>Sottovoce <span>call recorder</span></button>
  <div class="rec-panel">
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
  {#if notice}<div class="notice" role="alert"><span>{notice}</span><button onclick={() => notice = ''}>Dismiss</button></div>{/if}
  {#if !ready}<p>Connecting to recorder…</p>
  {:else if screen === 'home'}
   <header><h1>{recordingState === 'recording' ? 'Recording' : 'Ready when you are'}</h1><p>Your microphone and system audio are saved as separate tracks.</p></header>
   <section class="card capture">
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
   <header><h1>Settings</h1><p>Saved changes apply live. Manual edits to config.toml appear here too.</p></header>
   <form class="card settings" onsubmit={e => { e.preventDefault(); if (draft) void action(() => api.config({...draft!})); }}>
    <label>ElevenLabs API key<input type="password" autocomplete="off" bind:value={draft.elevenlabs_api_key} placeholder="Uses ELEVENLABS_API_KEY when empty" /></label>
    <label>Your name<input bind:value={draft.your_name} placeholder="You" /></label>
    <label>Meetings folder<input bind:value={draft.meetings_dir} placeholder="Documents\Meetings" /></label>
    <label>Transcripts folder<input bind:value={draft.transcripts_dir} placeholder="Default transcripts folder" /></label>
    <label>Archive folder<input bind:value={draft.archive_dir} placeholder="Default archive folder" /></label>
    <label>STT model<input bind:value={draft.stt_model} placeholder="scribe_v2" /></label>
    <label>Language<input bind:value={draft.language} placeholder="auto" /></label>
    <label class="checkbox"><input type="checkbox" checked={draft.auto_transcribe ?? false} onchange={e => { if(draft) draft.auto_transcribe = e.currentTarget.checked; }} />Auto-transcribe after Stop (paid batch API)</label>
    <label class="checkbox" title="Runs after recording when you press Transcribe. Nemotron finds voices on each side, labelled Remote 1, Remote 2…"><input type="checkbox" checked={draft.diarize ?? true} onchange={e => { if(draft) draft.diarize = e.currentTarget.checked; }} />Detect multiple voices</label>
    <button type="submit" disabled={closing}>Save settings</button>
   </form>
  {/if}
 </main>
</div>
{#if closing}<div class="closing" role="status"><div class="card"><h2>Finalizing…</h2><p>Saving audio and cancelling transcription before closing.</p></div></div>{/if}
