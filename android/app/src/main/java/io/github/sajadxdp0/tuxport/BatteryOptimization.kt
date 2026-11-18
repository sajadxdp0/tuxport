package io.github.sajadxdp0.tuxport

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Context
import android.os.PowerManager
import android.provider.Settings as SystemSettings

internal object BatteryOptimization {
    fun isExempt(context: Context): Boolean =
        context.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(context.packageName)

    // A user running guest services with the screen off can explicitly request the exemption.
    @SuppressLint("BatteryLife")
    fun requestExemption(activity: Activity) {
        if (Privacy.locked(activity) || isExempt(activity)) return
        try {
            activity.openPackageSettings(SystemSettings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
        } catch (_: ActivityNotFoundException) {
            openAppSettings(activity)
        }
    }

    fun openAppSettings(activity: Activity) {
        if (Privacy.locked(activity) || isExempt(activity)) return
        activity.openPackageSettings(SystemSettings.ACTION_APPLICATION_DETAILS_SETTINGS)
    }
}
