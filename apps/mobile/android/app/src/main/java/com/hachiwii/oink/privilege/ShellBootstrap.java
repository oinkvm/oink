package com.hachiwii.oink.privilege;

import android.content.Context;
import android.os.*;
import java.lang.reflect.*;

/** Shell uses the external-provider path: app_process has no AMS ApplicationThread record. */
final class ShellBootstrap {
    private ShellBootstrap() {}
    static Bundle attach(Context shell, Bundle request) throws Exception {
        String authority="com.hachiwii.oink.privilege";
        Object am=Class.forName("android.app.ActivityManager").getMethod("getService").invoke(null);
        Binder token=new Binder();
        Object holder=am.getClass().getMethod("getContentProviderExternal",String.class,int.class,IBinder.class,String.class)
            .invoke(am,authority,0,token,"Oink helper bootstrap");
        if(holder==null) throw new IllegalStateException("bootstrap provider unavailable");
        try {
            Object provider=holder.getClass().getField("provider").get(holder);
            for(Method method:provider.getClass().getMethods()) {
                if(!method.getName().equals("call")) continue;
                method.setAccessible(true);
                Class<?>[] types=method.getParameterTypes();
                if(types.length==5 && types[0].getName().equals("android.content.AttributionSource")) {
                    Object attribution=Context.class.getMethod("getAttributionSource").invoke(shell);
                    return (Bundle)method.invoke(provider,attribution,authority,"attach",null,request);
                }
                if(types.length==6 && types[0]==String.class) {
                    return (Bundle)method.invoke(provider,"com.android.shell",null,authority,"attach",null,request);
                }
            }
            throw new NoSuchMethodException("IContentProvider.call");
        } finally {
            am.getClass().getMethod("removeContentProviderExternalAsUser",String.class,IBinder.class,int.class).invoke(am,authority,token,0);
        }
    }
}
