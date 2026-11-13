package com.hachiwii.oink.privilege;

import android.app.Application;
import android.content.Context;
import android.os.Build;
import java.lang.reflect.*;

/** Framework bootstrap for app_process, whose default Context belongs to android, not shell. */
public final class ShellContext {
    private ShellContext() {}
    private static void field(Object target, Class<?> type, String name, Object value) throws Exception {
        Field f=type.getDeclaredField(name); f.setAccessible(true); f.set(target,value);
    }
    public static Context create() throws Exception {
        Class<?> threadClass=Class.forName("android.app.ActivityThread");
        Object thread=threadClass.getMethod("systemMain").invoke(null);
        Context system=(Context)threadClass.getMethod("getSystemContext").invoke(thread);
        Context shell=system.createPackageContext("com.android.shell",0);
        // createPackageContext inherits its container's attribution ("android"). On API35 this
        // makes ContentProvider bootstrap fail UID/package validation; VDM also lacks an
        // Application context. Set the process's real shell identity, never impersonate the App.
        Class<?> impl=Class.forName("android.app.ContextImpl");
        field(shell,impl,"mBasePackageName","com.android.shell");
        field(shell,impl,"mOpPackageName","com.android.shell");
        if(Build.VERSION.SDK_INT>=31) {
            Class<?> builder=Class.forName("android.content.AttributionSource$Builder");
            Object b=builder.getConstructor(int.class).newInstance(android.os.Process.myUid());
            builder.getMethod("setPackageName",String.class).invoke(b,"com.android.shell");
            field(shell,impl,"mAttributionSource",builder.getMethod("build").invoke(b));
        }
        field(shell.getContentResolver(),android.content.ContentResolver.class,"mPackageName","com.android.shell");
        Application app=new Application();
        Method attach=Application.class.getDeclaredMethod("attach",Context.class); attach.setAccessible(true); attach.invoke(app,shell);
        Field packageField=impl.getDeclaredField("mPackageInfo");packageField.setAccessible(true);
        Object loaded=packageField.get(shell);field(loaded,loaded.getClass(),"mApplication",app);
        field(thread,threadClass,"mInitialApplication",app);
        return shell;
    }
}
