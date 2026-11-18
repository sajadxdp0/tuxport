# Keep the complete app JNI interface, including console methods used only by instrumentation.
-keepclasseswithmembers,includedescriptorclasses class io.github.sajadxdp0.tuxport.* {
    native <methods>;
}

# The native terminal pump looks these up by name and JVM descriptor (hosts/android/src/vt.rs).
-keepclassmembers class io.github.sajadxdp0.tuxport.VtSession {
    public void on*(...);
}
