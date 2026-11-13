package com.hachiwii.oink.privilege;

import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.system.*;
import java.io.*;
import java.nio.*;
import java.util.*;
import java.util.regex.*;

/** Observe evdev directly: no EVIOCGRAB, interception or long-lived child process. */
final class VolumeKeyObserver implements AutoCloseable {
    private final OutputCapture.Events events;
    private volatile boolean closed;
    private Process discovery;
    private ParcelFileDescriptor[] wake;
    private Thread worker;
    VolumeKeyObserver(OutputCapture.Events events) { this.events = events; }
    void start() {
        worker = new Thread(this::observe, "OinkVolumeKeys");
        worker.start();
    }
    private void observe() {
        List<FileDescriptor> devices = new ArrayList<>();
        try {
            Process discover;
            synchronized (this) {
                if (closed) return;
                // getevent -lp is only a finite capability query. Live reads stay in this process,
                // so SIGKILL closes every evdev descriptor instead of leaving orphan input readers.
                discovery = new ProcessBuilder("getevent", "-lp").redirectErrorStream(true).start();
                discover = discovery;
            }
            List<String> paths;
            try (BufferedReader input = new BufferedReader(new InputStreamReader(discover.getInputStream()))) {
                paths = volumeDevices(input);
            }
            if (discover.waitFor() != 0 || closed) return;
            for (String path : paths) {
                try { devices.add(Os.open(path, OsConstants.O_RDONLY | OsConstants.O_NONBLOCK | OsConstants.O_CLOEXEC, 0)); }
                catch (ErrnoException e) { unavailable(e); }
            }
            if (devices.isEmpty()) return;
            synchronized (this) {
                if (closed) return;
                wake = ParcelFileDescriptor.createPipe();
            }
            StructPollfd[] polls = new StructPollfd[devices.size() + 1];
            for (int i = 0; i < polls.length; i++) {
                polls[i] = new StructPollfd();
                polls[i].fd = i == devices.size() ? wake[0].getFileDescriptor() : devices.get(i);
                polls[i].events = (short) OsConstants.POLLIN;
            }
            // Linux input_event ends in type:u16, code:u16, value:i32; the preceding timeval
            // uses native longs (16 bytes on 32-bit app_process, 24 on 64-bit). No device names,
            // key injection, input grabs or unrelated input-device reads are needed.
            int eventBytes = android.os.Process.is64Bit() ? 24 : 16;
            byte[] bytes = new byte[eventBytes * 32];
            ByteBuffer packet = ByteBuffer.wrap(bytes).order(ByteOrder.nativeOrder());
            while (!closed) {
                try { Os.poll(polls, -1); }
                catch (ErrnoException e) { if (e.errno == OsConstants.EINTR) continue; throw e; }
                if (closed || polls[devices.size()].revents != 0) break;
                for (int i = 0; i < devices.size() && !closed; i++) {
                    if ((polls[i].revents & OsConstants.POLLIN) == 0) {
                        if (polls[i].revents != 0) throw new IOException("evdev removed");
                        continue;
                    }
                    int count;
                    try { count = Os.read(devices.get(i), bytes, 0, bytes.length); }
                    catch (ErrnoException e) { if (e.errno == OsConstants.EAGAIN || e.errno == OsConstants.EINTR) continue; throw e; }
                    if (count <= 0 || count % eventBytes != 0) throw new IOException("invalid evdev record");
                    for (int offset = 0; offset < count; offset += eventBytes) {
                        packet.position(offset + eventBytes - 8);
                        Bundle event = keyEvent(packet.getShort() & 0xffff, packet.getShort() & 0xffff, packet.getInt());
                        if (event != null && !closed) events.event(event);
                    }
                }
            }
        } catch (Exception e) { unavailable(e); }
        finally {
            for (FileDescriptor device : devices) try { Os.close(device); } catch (ErrnoException ignored) { }
            synchronized (this) {
                if (discovery != null) { discovery.destroy(); discovery = null; }
                if (wake != null) {
                    for (ParcelFileDescriptor fd : wake) try { fd.close(); } catch (IOException ignored) { }
                    wake = null;
                }
            }
        }
    }
    private void unavailable(Exception e) {
        // Some ROMs do not grant shell evdev access. Capture remains usable without keys.
        if (!closed) android.util.Log.w("OinkPrivilege", "volume-key observation unavailable", e);
    }
    static List<String> volumeDevices(BufferedReader input) throws IOException {
        List<String> devices = new ArrayList<>();
        String path = null, line;
        while ((line = input.readLine()) != null) {
            Matcher device = Pattern.compile("add device \\d+: (/dev/input/event\\d+)").matcher(line);
            if (device.find()) path = device.group(1);
            if (path != null && (line.contains("KEY_VOLUMEUP") || line.contains("KEY_VOLUMEDOWN"))) {
                if (!devices.contains(path)) devices.add(path);
            }
        }
        return devices;
    }
    static Bundle keyEvent(int type, int code, int value) {
        if (type != 1 || (code != 114 && code != 115) || value < 0 || value > 2) return null;
        Bundle event = PrivilegeWire.result("output_volume_key");
        event.putInt("usage", code == 115 ? 0xe9 : 0xea);
        event.putBoolean("pressed", value != 0);
        return event;
    }
    @Override public void close() {
        synchronized (this) {
            if (!closed) {
                closed = true;
                if (discovery != null) discovery.destroy();
                if (wake != null) try { Os.write(wake[1].getFileDescriptor(), new byte[]{1}, 0, 1); }
                catch (Exception ignored) { }
            }
        }
        if (worker != null && worker != Thread.currentThread()) {
            boolean interrupted = false;
            for (;;) try { worker.join(); break; } catch (InterruptedException e) { interrupted = true; }
            if (interrupted) Thread.currentThread().interrupt();
        }
    }
}
