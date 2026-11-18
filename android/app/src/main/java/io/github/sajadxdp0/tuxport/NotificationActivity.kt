package io.github.sajadxdp0.tuxport

import android.content.Intent
import android.os.Bundle
import android.widget.FrameLayout
import androidx.fragment.app.FragmentActivity

/** Copies a notification's report after the shared privacy gate opens. */
class NotificationActivity : FragmentActivity(), Privacy.Listener {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(FrameLayout(this))
    }

    override fun onPrivacyUnlocked() {
        if (isFinishing) return
        if (intent.action == Notifications.ACTION_COPY_REPORT) {
            Notifications.copyReport(this, intent)
        }
        intent = Intent()
        finish()
    }
}
