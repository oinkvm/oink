package com.hachiwii.oink.privilege;

import android.content.Context;
import android.os.SystemClock;
import android.view.*;

/** Absolute SOURCE_MOUSE injection, never touchscreen emulation. Coordinates are owned by Rust. */
final class AbsoluteMouse implements AutoCloseable {
    private final DrawnPointer pointer;
    private final Object input;
    private float x,y;
    private int buttons;
    private long downTime;
    AbsoluteMouse(Context context) throws Exception {
        input=Class.forName("android.hardware.input.InputManager").getMethod("getInstance").invoke(null);
        pointer=new DrawnPointer(context);
    }
    private void event(int action,int actionButton,float vertical,float horizontal) throws Exception {
        MotionEvent.PointerProperties property=new MotionEvent.PointerProperties();property.id=0;property.toolType=MotionEvent.TOOL_TYPE_MOUSE;
        MotionEvent.PointerCoords coords=new MotionEvent.PointerCoords();coords.x=x;coords.y=y;coords.pressure=buttons==0?0:1;
        coords.setAxisValue(MotionEvent.AXIS_VSCROLL,vertical);coords.setAxisValue(MotionEvent.AXIS_HSCROLL,horizontal);
        MotionEvent e=MotionEvent.obtain(downTime,SystemClock.uptimeMillis(),action,1,new MotionEvent.PointerProperties[]{property},new MotionEvent.PointerCoords[]{coords},0,buttons,1,1,0,0,InputDevice.SOURCE_MOUSE,0);
        try {
            if(actionButton!=0)MotionEvent.class.getMethod("setActionButton",int.class).invoke(e,actionButton);
            if(!(boolean)VirtualPointer.invoke(input,"injectInputEvent",new Class[]{InputEvent.class,int.class},e,0))throw new IllegalStateException("mouse injection rejected");
        } finally { e.recycle(); }
    }
    void update(float x,float y,int desired,float wheel,float horizontal) throws Exception {
        if(!Float.isFinite(x)||!Float.isFinite(y)||(desired&~7)!=0||!Float.isFinite(wheel)||!Float.isFinite(horizontal))throw new IllegalArgumentException("mouse report");
        this.x=x;this.y=y;
        // This is a compositor-only layer, not an input window: keeping it visible does not
        // mark mouse events OBSCURED. Hiding it here made every press/drag lose its cursor.
        pointer.position(x,y);
        int old=buttons;
        if(old==0 && desired!=0){downTime=SystemClock.uptimeMillis();buttons=desired;event(MotionEvent.ACTION_DOWN,0,0,0);}
        for(int bit:new int[]{1,2,4})if(((old^desired)&bit)!=0){buttons=(buttons&~bit)|(desired&bit);event((desired&bit)!=0?MotionEvent.ACTION_BUTTON_PRESS:MotionEvent.ACTION_BUTTON_RELEASE,bit,0,0);}
        buttons=desired;
        if(old!=0 && desired==0)event(MotionEvent.ACTION_UP,0,0,0);
        else event(desired==0?MotionEvent.ACTION_HOVER_MOVE:MotionEvent.ACTION_MOVE,0,0,0);
        if(wheel!=0||horizontal!=0)event(MotionEvent.ACTION_SCROLL,0,wheel,horizontal);
    }
    void releaseAll() throws Exception { if(buttons!=0)update(x,y,0,0,0);pointer.hide(); }
    @Override public void close() throws Exception { try { releaseAll(); } finally { pointer.close(); } }
}
