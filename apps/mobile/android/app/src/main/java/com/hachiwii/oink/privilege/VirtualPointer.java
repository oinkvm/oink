package com.hachiwii.oink.privilege;

import android.content.Context;
import java.util.List;

/** Shared reflection and removal of the helper-owned companion association. */
final class VirtualPointer {
    static final String ASSOCIATION_ADDRESS="02:4f:49:4e:4b:01";
    static final String PROFILE="android.app.role.COMPANION_DEVICE_APP_STREAMING";
    static Object invoke(Object object,String name,Class<?>[] types,Object... args) throws Exception {
        return object.getClass().getMethod(name,types).invoke(object,args);
    }
    static Object builder(String name) throws Exception { return Class.forName(name+"$Builder").getConstructor().newInstance(); }
    /** The dedicated association's id, or -1. */
    static int findAssociation(Object cdm) throws Exception {
        for(Object entry:(List<?>)invoke(cdm,"getMyAssociations",new Class[]{})) {
            Object mac=invoke(entry,"getDeviceMacAddress",new Class[]{});
            if(mac!=null && ASSOCIATION_ADDRESS.equalsIgnoreCase(mac.toString()) && PROFILE.equals(invoke(entry,"getDeviceProfile",new Class[]{})))
                return (int)invoke(entry,"getId",new Class[]{});
        }
        return -1;
    }
    /** Run the shell command to remove this helper's association when privilege is stopped. */
    static String companionCommand(String... args) throws Exception {
        String[] command=new String[args.length+2];command[0]="cmd";command[1]="companiondevice";
        System.arraycopy(args,0,command,2,args.length);
        Process process=new ProcessBuilder(command).redirectErrorStream(true).start();
        process.getOutputStream().close();
        java.io.ByteArrayOutputStream bytes=new java.io.ByteArrayOutputStream();
        try(java.io.InputStream in=process.getInputStream()) {
            byte[] buffer=new byte[1024]; for(int n;(n=in.read(buffer))>0;) bytes.write(buffer,0,n);
        }
        String output=new String(bytes.toByteArray(),java.nio.charset.StandardCharsets.UTF_8).trim();
        int exit=process.waitFor();
        return exit==0 ? null : "cmd companiondevice "+args[0]+" exit "+exit+(output.isEmpty()?"":": "+output);
    }
    /** Privileged mode turned off: the association is Oink's own, nothing else may keep using it. */
    static void removeAssociation(Context context) {
        try {
            Object cdm=context.getSystemService(Class.forName("android.companion.CompanionDeviceManager"));
            if(cdm==null || findAssociation(cdm)<0)return;
            String failure=companionCommand("disassociate","0","com.android.shell",ASSOCIATION_ADDRESS);
            if(failure!=null)android.util.Log.w("OinkPrivilege","pointer association not removed: "+failure);
        } catch(Exception e) { android.util.Log.w("OinkPrivilege","pointer association not removed",e); }
    }
}
