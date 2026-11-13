package com.hachiwii.oink.privilege;

import android.content.Context;
import android.media.*;
import android.os.*;
import android.system.*;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.concurrent.atomic.AtomicBoolean;

/** Shell-only raw capture. No codec, gain, audio focus, system-volume writes or network owner. */
public final class OutputCapture implements AutoCloseable {
    public static final int RATE = 48000, CHANNELS = 2, FRAMES = 240;
    // Local pipe record: little endian size:u32, first-frame:u64, MONOTONIC-ns:u64,
    // discontinuity:u32, then interleaved PCM16. Always <= PIPE_BUF for atomic nonblocking writes.
    public static final int HEADER_BYTES = 24, PCM_BYTES = FRAMES * CHANNELS * 2;
    public interface Events { void event(Bundle event); }
    private final AudioManager manager;
    private final Context context;
    private final ParcelFileDescriptor pipe;
    private final Events events;
    private final long generation;
    private final AtomicBoolean closed = new AtomicBoolean();
    private Object policy;
    private volatile AudioRecord record;
    private Thread worker;
    private VolumeKeyObserver keys;
    private boolean registered;
    private volatile long droppedBlocks;
    public long droppedBlocks() { return droppedBlocks; }

    private OutputCapture(Context context, ParcelFileDescriptor pipe, long generation, Events events) {
        this.context = context;
        this.manager = context.getSystemService(AudioManager.class);
        this.pipe = pipe;
        this.generation = generation;
        this.events = events;
    }
    private void start() throws Exception {
        try {
            AudioFormat format = new AudioFormat.Builder().setSampleRate(RATE)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT).setChannelMask(AudioFormat.CHANNEL_OUT_STEREO).build();
            // AudioPolicy is hidden SDK API, but shell holds MODIFY_AUDIO_ROUTING. LOOP_BACK alone
            // steals only MEDIA/GAME, leaving alerts/calls on the phone. REMOTE_SUBMIX address "0"
            // instead records silence on HyperOS Android 15; the policy mix gives raw fixed gain,
            // including when locked, at volume zero and in vibrate mode (measured on API35).
            Class<?> ruleBuilder = Class.forName("android.media.audiopolicy.AudioMixingRule$Builder");
            Object rb = ruleBuilder.getConstructor().newInstance();
            for (int usage : new int[]{AudioAttributes.USAGE_MEDIA, AudioAttributes.USAGE_GAME})
                ruleBuilder.getMethod("addMixRule", int.class, Object.class).invoke(rb, 1,
                    new AudioAttributes.Builder().setUsage(usage).build());
            Object rule = ruleBuilder.getMethod("build").invoke(rb);
            Class<?> mixBuilder = Class.forName("android.media.audiopolicy.AudioMix$Builder");
            Object mb = mixBuilder.getConstructor(rule.getClass()).newInstance(rule);
            mixBuilder.getMethod("setFormat", AudioFormat.class).invoke(mb, format);
            mixBuilder.getMethod("setRouteFlags", int.class).invoke(mb, 2); // LOOP_BACK, never RENDER
            Object mix = mixBuilder.getMethod("build").invoke(mb);
            Class<?> builder = Class.forName("android.media.audiopolicy.AudioPolicy$Builder");
            Object pb = builder.getConstructor(Context.class).newInstance(context);
            builder.getMethod("addMix", mix.getClass()).invoke(pb, mix);
            builder.getMethod("setLooper", Looper.class).invoke(pb, Looper.getMainLooper());
            policy = builder.getMethod("build").invoke(pb);
            int result = (Integer) AudioManager.class.getMethod("registerAudioPolicy", policy.getClass()).invoke(manager, policy);
            if (result != 0) throw new CaptureBusyException();
            registered = true;
            record = (AudioRecord) policy.getClass().getMethod("createAudioRecordSink", mix.getClass()).invoke(policy, mix);
            if (record == null || record.getState() != AudioRecord.STATE_INITIALIZED) throw new CaptureBusyException();
            try { record.startRecording(); }
            catch (IllegalStateException e) { throw new CaptureBusyException(); }
            if (record.getRecordingState() != AudioRecord.RECORDSTATE_RECORDING) throw new CaptureBusyException();
            int flags = Os.fcntlInt(pipe.getFileDescriptor(), OsConstants.F_GETFL, 0);
            Os.fcntlInt(pipe.getFileDescriptor(), OsConstants.F_SETFL, flags | OsConstants.O_NONBLOCK);
            keys = new VolumeKeyObserver(this::emit);
            keys.start();
            worker = new Thread(this::capture, "OinkOutputCapture");
            worker.start();
        } catch (Exception e) { close(); throw e; }
    }
    public static OutputCapture open(Context context, ParcelFileDescriptor pipe, long generation, Events events) throws Exception {
        OutputCapture capture = new OutputCapture(context, pipe, generation, events);
        capture.start();
        return capture;
    }
    private void emit(Bundle event) {
        if (closed.get()) return;
        event.putLong("generation", generation);
        events.event(event);
    }
    private void capture() {
        try {
            android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_AUDIO);
            short[] samples = new short[FRAMES * CHANNELS];
            ByteBuffer packet = ByteBuffer.allocate(HEADER_BYTES + PCM_BYTES).order(ByteOrder.LITTLE_ENDIAN);
            AudioTimestamp timestamp = new AudioTimestamp();
            long position = 0;
            boolean gap = true;
            while (!closed.get()) {
                int used = 0;
                while (used < samples.length && !closed.get()) {
                    int n = record.read(samples, used, samples.length - used, AudioRecord.READ_BLOCKING);
                    if (n <= 0) throw new java.io.IOException("capture read failed");
                    used += n;
                }
                if (closed.get()) break;
                long hostNs = 0;
                if (record.getTimestamp(timestamp, AudioTimestamp.TIMEBASE_MONOTONIC) == AudioRecord.SUCCESS)
                    hostNs = timestamp.nanoTime + (position - timestamp.framePosition) * 1_000_000_000L / RATE;
                packet.clear();
                packet.putInt(PCM_BYTES).putLong(position).putLong(Math.max(0, hostNs)).putInt(gap ? 1 : 0);
                for (short sample : samples) packet.putShort(sample);
                try {
                    int written = Os.write(pipe.getFileDescriptor(), packet.array(), 0, packet.capacity());
                    if (written != packet.capacity()) throw new java.io.IOException("partial pipe record");
                    gap = false;
                } catch (ErrnoException e) {
                    if (e.errno != OsConstants.EAGAIN) throw e;
                    droppedBlocks++;
                    gap = true; // Full pipe: drop this entire record, never stall AudioRecord.
                }
                position += FRAMES;
            }
        } catch (Exception e) {
            if (!closed.get()) {
                Bundle event = PrivilegeWire.result("output_capture_failed");
                event.putString("reason", "interrupted");
                emit(event);
            }
        } finally { release(); }
    }
    /** Stop recording wakes a blocking read; worker owns final release and closes the pipe (EOF). */
    @Override public void close() {
        if (closed.compareAndSet(false, true)) {
            if (keys != null) keys.close();
            AudioRecord current = record;
            try { if (current != null) current.stop(); } catch (IllegalStateException ignored) { }
        }
        if (worker != null && worker != Thread.currentThread()) {
            boolean interrupted = false;
            for (;;) try { worker.join(); break; } catch (InterruptedException e) { interrupted = true; }
            if (interrupted) Thread.currentThread().interrupt();
        } else if (worker == null) release();
    }
    private synchronized void release() {
        closed.set(true);
        if (keys != null) keys.close();
        if (record != null) { record.release(); record = null; }
        if (registered) {
            try { AudioManager.class.getMethod("unregisterAudioPolicy", policy.getClass()).invoke(manager, policy); }
            catch (Exception e) { android.util.Log.e("OinkPrivilege", "unregister capture policy failed", e); }
            registered = false;
        }
        try { pipe.close(); } catch (java.io.IOException ignored) { }
    }
    public static final class CaptureBusyException extends Exception { }
}
