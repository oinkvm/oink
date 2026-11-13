package com.hachiwii.oink.privilege;

import android.content.Context;
import android.content.res.Resources;
import android.graphics.*;
import android.view.PointerIcon;
import android.view.Surface;
import java.lang.reflect.Field;

/** Pure compositor layer: no input window, focus or touch region. Shell does not own an AMS window session. */
final class DrawnPointer implements AutoCloseable {
    private final Class<?> controlType=Class.forName("android.view.SurfaceControl");
    private Object control,transaction;
    private Surface surface;
    private final float hotSpotX,hotSpotY;
    DrawnPointer(Context context) throws Exception {
        Arrow arrow=Arrow.load(context.getResources());
        hotSpotX=arrow.hotSpotX;hotSpotY=arrow.hotSpotY;
        Object b=VirtualPointer.builder("android.view.SurfaceControl");
        VirtualPointer.invoke(b,"setName",new Class[]{String.class},"Oink drawn pointer");
        VirtualPointer.invoke(b,"setBufferSize",new Class[]{int.class,int.class},arrow.bitmap.getWidth(),arrow.bitmap.getHeight());
        VirtualPointer.invoke(b,"setFormat",new Class[]{int.class},PixelFormat.TRANSLUCENT);
        control=VirtualPointer.invoke(b,"build",new Class[]{});
        try {
            surface=Surface.class.getConstructor(controlType).newInstance(control);
            transaction=Class.forName("android.view.SurfaceControl$Transaction").getConstructor().newInstance();
            // A buffer-only SurfaceControl still participates in input occlusion. On
            // HyperOS 3 / Android 16, InputDispatcher drops mouse events underneath this
            // very arrow as BLOCK_UNTRUSTED, despite its empty touch region and lack of
            // an input channel. Mark our shell-owned layer (and its BLAST child) trusted
            // before showing it. The API starts at Android 12, with untrusted-touch
            // blocking. This preserves the visible arrow through clicks and drags and
            // changes no other window's input policy. Android 15 previously accepted
            // the unmarked layer on the test phone.
            if(android.os.Build.VERSION.SDK_INT>=31)
                change("setTrustedOverlay",new Class[]{boolean.class},true);
            Canvas canvas=surface.lockCanvas(null);
            try {
                canvas.drawColor(Color.TRANSPARENT,PorterDuff.Mode.CLEAR);
                canvas.drawBitmap(arrow.bitmap,0,0,null);
            } finally { surface.unlockCanvasAndPost(canvas); }
            change("setLayerStack",new Class[]{int.class},0);change("setLayer",new Class[]{int.class},1000000);
        } catch(Exception failure) {
            try { close(); } catch(Exception cleanup) { failure.addSuppressed(cleanup); }
            throw failure;
        }
    }
    private void change(String method,Class<?>[] types,Object... values) throws Exception {
        Class<?>[] allTypes=new Class<?>[types.length+1];Object[] allValues=new Object[values.length+1];
        allTypes[0]=controlType;allValues[0]=control;System.arraycopy(types,0,allTypes,1,types.length);System.arraycopy(values,0,allValues,1,values.length);
        VirtualPointer.invoke(transaction,method,allTypes,allValues);
    }
    void position(float x,float y) throws Exception {
        change("setPosition",new Class[]{float.class,float.class},x-hotSpotX,y-hotSpotY);
        // Keep the trusted layer visible through DOWN/MOVE/UP. Hide only when input
        // focus is released or on teardown.
        change("show",new Class[]{});apply();
    }
    void hide() throws Exception { change("hide",new Class[]{});apply(); }
    private void apply() throws Exception { VirtualPointer.invoke(transaction,"apply",new Class[]{}); }
    private static final class Arrow {
        final Bitmap bitmap;
        final float hotSpotX,hotSpotY;
        Arrow(Bitmap bitmap,float hotSpotX,float hotSpotY) {
            this.bitmap=bitmap;this.hotSpotX=hotSpotX;this.hotSpotY=hotSpotY;
        }
        private static Object field(PointerIcon icon,String name) throws Exception {
            Field field=PointerIcon.class.getDeclaredField(name);field.setAccessible(true);return field.get(icon);
        }
        static Arrow load(Resources resources) {
            try {
                // getSystemIcon(TYPE_ARROW) is only a type token on Android 15; it has no pixels.
                // Load the framework's pointer-icon XML instead, letting Resources scale both the
                // bitmap and its declared tip hotspot at the display density. app_process runs as
                // shell and can read PointerIcon's hidden pixel/hotspot fields (tested on API35).
                // This does not create a native pointer or change system pointer settings.
                int resource=resources.getIdentifier("pointer_arrow_icon","drawable","android");
                if(resource==0)throw new Resources.NotFoundException("framework pointer_arrow_icon");
                PointerIcon icon=PointerIcon.load(resources,resource);
                Bitmap bitmap=(Bitmap)field(icon,"mBitmap");
                float x=(float)field(icon,"mHotSpotX"),y=(float)field(icon,"mHotSpotY");
                if(bitmap==null || !Float.isFinite(x) || !Float.isFinite(y) || x<0 || y<0 || x>=bitmap.getWidth() || y>=bitmap.getHeight())
                    throw new IllegalStateException("invalid framework arrow");
                return new Arrow(bitmap,x,y);
            } catch(Exception unavailable) {
                android.util.Log.w("OinkPrivilege","Framework arrow unavailable; drawing static arrow",unavailable);
                return draw(resources.getDisplayMetrics().density);
            }
        }
        static Arrow draw(float density) {
            Bitmap bitmap=Bitmap.createBitmap(Math.round(32*density),Math.round(32*density),Bitmap.Config.ARGB_8888);
            Canvas canvas=new Canvas(bitmap);canvas.scale(density,density);
            Path path=new Path();path.moveTo(5,4);path.lineTo(5,25);path.lineTo(10.5f,19.5f);
            path.lineTo(14.5f,28);path.lineTo(18,26.5f);path.lineTo(14,18);path.lineTo(21,18);path.close();
            Paint paint=new Paint(Paint.ANTI_ALIAS_FLAG);paint.setColor(Color.WHITE);
            paint.setStyle(Paint.Style.STROKE);paint.setStrokeWidth(2);paint.setStrokeJoin(Paint.Join.ROUND);
            paint.setShadowLayer(1.5f,0,1,0x66000000);canvas.drawPath(path,paint);
            paint.clearShadowLayer();paint.setStyle(Paint.Style.FILL);paint.setColor(Color.BLACK);canvas.drawPath(path,paint);
            return new Arrow(bitmap,5*density,4*density);
        }
    }
    @Override public void close() throws Exception {
        try { if(transaction!=null) { change("remove",new Class[]{});apply(); } }
        finally { if(surface!=null)surface.release();
            if(control!=null)VirtualPointer.invoke(control,"release",new Class[]{});
            if(transaction!=null)VirtualPointer.invoke(transaction,"close",new Class[]{}); }
    }
}
