package com.hachiwii.oink.privilege;

import java.io.*;
import java.nio.*;

/** Linux uhid.h packed CREATE2 / INPUT2. Closing the fd destroys the virtual device. */
final class UhidDevices implements AutoCloseable {
    private FileOutputStream keyboard;
    private static final byte[] KEYBOARD = bytes(5,1,9,6,161,1,5,7,25,224,41,231,21,0,37,1,117,1,149,8,129,2,149,1,117,8,129,1,149,6,117,8,21,0,38,223,0,5,7,25,0,42,223,0,129,0,192);
    static byte[] bytes(int... values) { byte[] b=new byte[values.length]; for(int i=0;i<b.length;i++) b[i]=(byte)values[i]; return b; }
    private static FileOutputStream create(String name, byte[] descriptor, int productId) throws IOException {
        FileOutputStream fd = new FileOutputStream("/dev/uhid");
        try {
            ByteBuffer b=ByteBuffer.allocate(4380).order(ByteOrder.LITTLE_ENDIAN);
            b.putInt(11); b.put(name.getBytes(java.nio.charset.StandardCharsets.US_ASCII)); b.position(260);
            b.putShort((short)descriptor.length); b.putShort((short)3);
            b.putInt(0x18d1); b.putInt(productId); b.putInt(1); b.putInt(0); b.put(descriptor);
            fd.write(b.array()); return fd;
        } catch(IOException e) { fd.close(); throw e; }
    }
    synchronized void openKeyboard() throws IOException {
        if (keyboard == null) keyboard=create("Oink keyboard", KEYBOARD, 0x4ee8);
    }
    private static void report(FileOutputStream fd, byte[] data) throws IOException {
        if (fd == null) throw new IOException("UHID unavailable");
        ByteBuffer b=ByteBuffer.allocate(6+data.length).order(ByteOrder.LITTLE_ENDIAN);
        b.putInt(12); b.putShort((short)data.length); b.put(data); fd.write(b.array());
    }
    synchronized void keyboard(int modifiers,byte[] usages) throws IOException {
        report(keyboard, keyboardReport(modifiers, usages));
    }
    /**
     * The 8-byte boot keyboard report. The App sends all six key slots, and an empty slot is 0
     * (HID "no event"). Rejecting 0 failed every key report with a free slot, which is nearly all of
     * them; the failed call turned privileged input off, so the PC's pointer left the phone at once
     * (2026-11-13). Usages 1-3 are HID error codes and are still refused.
     */
    static byte[] keyboardReport(int modifiers,byte[] usages) {
        if (modifiers<0 || modifiers>255 || usages==null || usages.length>6) throw new IllegalArgumentException("keyboard report range");
        byte[] report=new byte[8]; report[0]=(byte)modifiers;
        for(int i=0;i<usages.length;i++) {
            int usage=usages[i]&255;
            if(usage!=0 && (usage<4 || usage>223)) throw new IllegalArgumentException("keyboard usage");
            report[i+2]=usages[i];
        }
        return report;
    }
    synchronized void releaseAll() throws IOException {
        if(keyboard!=null) report(keyboard,new byte[8]);
    }
    @Override public synchronized void close() throws IOException {
        try { if(keyboard!=null)keyboard.close(); }
        finally { keyboard=null; }
    }
}
