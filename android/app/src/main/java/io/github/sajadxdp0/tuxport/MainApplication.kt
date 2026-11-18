package io.github.sajadxdp0.tuxport

import android.app.Application

/** Initializes app-wide components when the process starts. */
class MainApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        Privacy.install(this)
        Diagnostics.install(this)
    }
}
