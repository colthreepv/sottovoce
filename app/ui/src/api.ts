import { invoke, convertFileSrc } from '@tauri-apps/api/core';
export type RecordingState = 'ready' | 'starting' | 'recording' | 'finalizing';
export type JobState = 'queued' | 'running' | 'cancelling' | 'done' | 'cancelled' | { failed: string };
export interface Config {
 elevenlabs_api_key: string | null; stt_model: string | null; language: string | null;
 meetings_dir: string | null; transcripts_dir: string | null; archive_dir: string | null; your_name: string | null; diarize: boolean | null;
 auto_transcribe: boolean | null; mic_device: string | null; output_device: string | null;
}
export interface Device { id: string; name: string; is_default: boolean; default_format: string | null }
export interface Devices { inputs: Device[]; outputs: Device[] }
export interface Entry { dir: string; title: string; started_at_unix_ms: number; duration_ms: number; transcribed: boolean; status: string }
export interface MeetingEntry { meeting: Entry; job: JobState | null }
export interface Speaker { id: string; side: 'mic' | 'computer'; name: string }
export interface Utterance { speaker: string; side: 'mic' | 'computer'; start_ms: number; end_ms: number; text: string }
export interface Meeting { title: string; source_app: string | null; started_at_unix_ms: number; duration_ms: number; language: string | null; stt_model: string; speakers: Speaker[]; utterances: Utterance[] }
export interface MeetingView { meeting: Meeting; mic: string | null; system: string | null }
export interface Snapshot { config: Config; devices: Devices; state: RecordingState; elapsed_ms: number; recording_meeting: string | null; closing: boolean; meetings: MeetingEntry[] }
export type CoreEvent =
 | { type: 'levels'; mic: number; system: number }
 | { type: 'recording_state_changed'; state: RecordingState; elapsed_ms: number; meeting: string | null }
 | { type: 'job_queued' | 'job_done' | 'job_cancelled'; meeting: string }
 | { type: 'job_progress'; meeting: string; stage: string; progress: number }
 | { type: 'job_failed'; meeting: string; error: string }
 | { type: 'library_busy'; meeting: string; busy: boolean }
 | { type: 'library_done'; meeting: string; action: 'delete_audio' | 'archive'; output: string | null }
 | { type: 'meetings_changed' | 'shutdown_complete' }
 | { type: 'devices_changed'; devices: Devices }
 | { type: 'config_changed'; config: Config }
 | { type: 'error'; message: string };
export const api = {
 getConfig: () => invoke<Config>('get_config'),
 testKey: (key: string | null) => invoke<void>('test_api_key', {key}),
 folderDefaults: () => invoke<Record<'meetings_dir' | 'transcripts_dir' | 'archive_dir', string>>('get_folder_defaults'),
 snapshot: () => invoke<Snapshot>('get_snapshot'), meetings: () => invoke<MeetingEntry[]>('list_meetings'),
 meeting: (meeting: string) => invoke<MeetingView>('get_meeting', { meeting }),
 start: () => invoke<void>('start_recording'), stop: () => invoke<void>('stop_recording'),
 transcribe: (meeting: string) => invoke<void>('transcribe', { meeting }),
 cancel: (meeting: string) => invoke<void>('cancel_job', { meeting }),
 rename: (meeting: string, title: string) => invoke<string>('rename_meeting', { meeting, title }),
 deleteAudio: (meeting: string) => invoke<void>('delete_audio', { meeting }),
 archive: (meeting: string) => invoke<void>('archive_meeting', { meeting }),
 mic: (id: string | null) => invoke<void>('set_mic_device', { id }), output: (id: string | null) => invoke<void>('set_output_device', { id }),
 devices: () => invoke<void>('list_devices'), config: (config: Config) => {
  const normalized = {...config};
  for (const key of ['elevenlabs_api_key', 'stt_model', 'language', 'meetings_dir', 'transcripts_dir', 'archive_dir', 'your_name', 'mic_device', 'output_device'] as const) {
    normalized[key] = config[key]?.trim() || null;
  }
  return invoke<void>('update_config', { config: normalized });
 },
};
export const audioUrl = (path: string) => convertFileSrc(path);
export const clock = (ms: number) => {
 const seconds = Math.floor(ms / 1000); return `${Math.floor(seconds / 60).toString().padStart(2,'0')}:${(seconds % 60).toString().padStart(2,'0')}`;
};
export const meter = (peak: number) => Math.max(0, Math.min(100, ((20 * Math.log10(Math.max(peak, 0.001))) + 60) / 60 * 100));
export const busy = (job: JobState | null) => job === 'queued' || job === 'running' || job === 'cancelling';
export const badge = (job: JobState | null) => job === 'running' ? 'Transcribing' : typeof job === 'object' && job !== null ? 'Failed' : job ?? '';
