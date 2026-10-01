<script lang="ts">
 import { onDestroy } from 'svelte';
 import { audioUrl, clock } from './api';
 let { mic, system, durationMs, onerror }: { mic: string | null; system: string | null; durationMs: number; onerror: (error: string) => void } = $props();
 let micAudio = $state<HTMLAudioElement>(); let systemAudio = $state<HTMLAudioElement>();
 let playing = $state(false); let position = $state(0); let loading = $state(false);
 let mediaDuration = $state(0);
 let playAttempt = 0;
 let destroyed = false;
 let duration = $derived(Math.max(durationMs / 1000, mediaDuration));
 function tracks() { return [micAudio, systemAudio].filter((a): a is HTMLAudioElement => !!a); }
 function pause() { playAttempt++; tracks().forEach(a => a.pause()); playing = false; }
 export function seek(seconds: number) { position = Math.max(0, Math.min(duration, seconds)); for(const audio of tracks()) { if(audio.readyState > 0) audio.currentTime = position; } }
 async function toggle() {
   if (playing) { pause(); return; }
   if (position >= duration) seek(0);
   loading = true;
   const attempt = ++playAttempt;
   const currentTracks = tracks();
   try { for (const a of currentTracks) a.currentTime = position; await Promise.all(currentTracks.map(a => a.play())); if (destroyed || attempt !== playAttempt) { currentTracks.forEach(a => a.pause()); return; } playing = true; }
   catch(error) { pause(); onerror(`Playback failed: ${error}`); }
   finally { loading = false; }
 }
 function tick(event: Event) {
   const primary = micAudio ?? systemAudio; const source = event.currentTarget as HTMLAudioElement;
   if (source !== primary && primary && !primary.ended) return;
   position = source.currentTime;
   if (playing) for (const a of tracks()) if (a !== source && Math.abs(a.currentTime - position) > 0.15 && !a.ended) a.currentTime = position;
 }
 function metadata(event: Event) { const a = event.currentTarget as HTMLAudioElement; if(Number.isFinite(a.duration)) mediaDuration = Math.max(mediaDuration, a.duration); }
 onDestroy(() => { destroyed = true; pause(); });
</script>
<section class="card player" aria-label="Audio player">
 {#if mic}<audio bind:this={micAudio} src={audioUrl(mic)} preload="metadata" ontimeupdate={tick} onloadedmetadata={metadata} onended={() => { if(!systemAudio || systemAudio.ended) pause(); }} onerror={() => onerror('Could not load microphone audio')} ></audio>{/if}
 {#if system}<audio bind:this={systemAudio} src={audioUrl(system)} preload="metadata" ontimeupdate={tick} onloadedmetadata={metadata} onended={() => { if(!micAudio || micAudio.ended) pause(); }} onerror={() => onerror('Could not load system audio')} ></audio>{/if}
 {#if mic || system}
  <div class="player-top"><button onclick={toggle} disabled={loading}>{loading ? 'Loading…' : playing ? 'Pause' : 'Play'}</button><span class="timer">{clock(position * 1000)} / {clock(duration * 1000)}</span></div>
  <input aria-label="Seek audio" type="range" min="0" max={duration} step="0.1" value={position} oninput={e => seek(Number(e.currentTarget.value))} />
  <div class="skip">{#each [-15,-5,5,15] as amount}<button onclick={() => seek(position + amount)}>{amount > 0 ? '+' : ''}{amount}s</button>{/each}</div>
 {:else}<p class="empty">Audio is unavailable for this meeting.</p>{/if}
</section>
