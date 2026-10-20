package org.matrixaxon.axon.remotepush

import android.Manifest
import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import android.webkit.WebView
import java.security.MessageDigest
import androidx.core.app.NotificationCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

private const val CHANNEL_ID = "messages-v2"
private const val COLLAPSE_ID = Regex("^[0-9a-f]{32}$")

@InvokeArg
class PostLocalArgs {
    lateinit var title: String
    lateinit var body: String
    lateinit var collapseId: String
    lateinit var accountId: String
    lateinit var roomId: String
    lateinit var eventId: String
    var threadRootId: String? = null
}

/**
 * FCM registration and a local notification that uses the same tag the
 * server puts on the remote alert.
 *
 * The registration token is returned to the page and never logged. A
 * message that arrives while this process is in the foreground is not
 * shown: the page's local notifier owns that toast. There is no foreground
 * service; Doze is allowed to stop the process, and FCM wakes it.
 */
@TauriPlugin
class RemotePushPlugin(private val activity: Activity) : Plugin(activity) {
    private var pendingWake: JSObject? = null

    companion object {
        var instance: RemotePushPlugin? = null

        /**
         * Set from the activity lifecycle. A background FCM message is posted
         * by this process so `event_id` is on the notification extras. The
         * system-drawn alert keeps that id only on the content intent, which
         * `activeNotifications` cannot read.
         */
        @Volatile
        var processInForeground: Boolean = false
    }

    override fun onResume() {
        processInForeground = true
    }

    override fun onPause() {
        processInForeground = false
    }

    override fun load(webView: WebView) {
        instance = this
        super.load(webView)
        deliver(activity.intent)
    }

    override fun onNewIntent(intent: Intent) {
        deliver(intent)
    }

    @Command
    fun deviceToken(invoke: Invoke) {
        val apps = try {
            FirebaseApp.getApps(activity)
        } catch (_: Exception) {
            emptyList<FirebaseApp>()
        }
        if (apps.isEmpty()) {
            invoke.reject(
                "Firebase is not configured. Add google-services.json beside src-tauri and rebuild.",
            )
            return
        }
        try {
            FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
                activity.runOnUiThread {
                    val token = if (task.isSuccessful) task.result else null
                    if (token.isNullOrEmpty()) {
                        invoke.reject("could not read the FCM registration token")
                    } else {
                        val result = JSObject()
                        result.put("token", token)
                        invoke.resolve(result)
                    }
                }
            }
        } catch (_: Exception) {
            invoke.reject("could not read the FCM registration token")
        }
    }

    @Command
    fun deliveredEventIds(invoke: Invoke) {
        val manager = activity.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val ids = JSObject()
        val list = org.json.JSONArray()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            for (active in manager.activeNotifications) {
                val eventId = active.notification.extras?.getString("event_id")
                if (!eventId.isNullOrEmpty()) {
                    list.put(eventId)
                }
            }
        }
        ids.put("eventIds", list)
        invoke.resolve(ids)
    }

    @Command
    fun pendingWake(invoke: Invoke) {
        val wake = pendingWake
        pendingWake = null
        invoke.resolve(wake)
    }

    @Command
    fun postLocal(invoke: Invoke) {
        val args = try {
            invoke.parseArgs(PostLocalArgs::class.java)
        } catch (_: Exception) {
            invoke.reject("the local notification was missing a field")
            return
        }
        if (!COLLAPSE_ID.matches(args.collapseId)) {
            invoke.reject("the collapse id is not 32 hex characters")
            return
        }
        if (!canPost()) {
            invoke.reject("notification permission is not granted")
            return
        }
        postCollapsedNotification(
            activity,
            args.title,
            args.body,
            args.collapseId,
            args.eventId,
            contentIntent(args),
        )
        invoke.resolve()
    }

    private fun deliver(intent: Intent?) {
        val wake = wakeFrom(intent) ?: return
        // Kept until pendingWake is read. A document reload asks again; the
        // page ignores an event id it already opened in this webview session.
        pendingWake = wake
        trigger("wake", wake)
    }

    private fun canPost(): Boolean = canPostNotifications(activity)

    private fun contentIntent(args: PostLocalArgs): PendingIntent {
        val intent = Intent(activity, activity.javaClass).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
            putExtra("account_id", args.accountId)
            putExtra("room_id", args.roomId)
            putExtra("event_id", args.eventId)
            if (args.threadRootId != null) {
                putExtra("thread_root_id", args.threadRootId)
            }
        }
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        return PendingIntent.getActivity(activity, args.collapseId.hashCode(), intent, flags)
    }
}

class AxonFirebaseMessagingService : FirebaseMessagingService() {
    override fun handleIntent(intent: Intent) {
        if (
            intent.action == "com.google.firebase.messaging.NEW_TOKEN" ||
            intent.extras == null ||
            RemotePushPlugin.processInForeground
        ) {
            super.handleIntent(intent)
            return
        }
        val message = try {
            RemoteMessage(intent.extras!!)
        } catch (_: Exception) {
            super.handleIntent(intent)
            return
        }
        val eventId = firstText(message.data["event_id"], intent.getStringExtra("event_id"))
        val account = firstText(message.data["account_id"], intent.getStringExtra("account_id"))
        val room = firstText(message.data["room_id"], intent.getStringExtra("room_id"))
        if (eventId.isEmpty() || account.isEmpty() || room.isEmpty() || !canPostNotifications(this)) {
            super.handleIntent(intent)
            return
        }
        val title = message.notification?.title?.takeIf { it.isNotBlank() }
            ?: intent.getStringExtra("gcm.notification.title")
            ?: "Axon"
        val body = message.notification?.body?.takeIf { it.isNotBlank() }
            ?: intent.getStringExtra("gcm.notification.body")
            ?: "New activity"
        val thread = firstText(
            message.data["thread_root_id"],
            intent.getStringExtra("thread_root_id"),
        )
        val launch = packageManager.getLaunchIntentForPackage(packageName) ?: run {
            super.handleIntent(intent)
            return
        }
        launch.flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        launch.putExtra("account_id", account)
        launch.putExtra("room_id", room)
        launch.putExtra("event_id", eventId)
        if (thread.isNotEmpty()) {
            launch.putExtra("thread_root_id", thread)
        }
        val pending = PendingIntent.getActivity(
            this,
            collapseTag(room, intent.getStringExtra("gcm.notification.tag")).hashCode(),
            launch,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        // Do not call super. The SDK would draw a second alert whose extras
        // omit event_id, and the resume read would miss it.
        postCollapsedNotification(
            this,
            title,
            body,
            collapseTag(
                room,
                intent.getStringExtra("gcm.notification.tag")
                    ?: intent.getStringExtra("gcm.n.tag"),
            ),
            eventId,
            pending,
        )
    }

    override fun onMessageReceived(message: RemoteMessage) {
        // Foreground messages reach here via handleIntent's super call.
        // The page posts the local toast. Showing this too would double it.
    }

    override fun onNewToken(token: String) {
        // The next registration reads the current token from Firebase.
        // Nothing here stores it in a log or a file.
    }
}

private fun postCollapsedNotification(
    context: Context,
    title: String,
    body: String,
    collapseId: String,
    eventId: String,
    contentIntent: PendingIntent,
) {
    val manager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    ensureChannel(manager)
    val notification = NotificationCompat.Builder(context, CHANNEL_ID)
        .setSmallIcon(smallIcon(context))
        .setContentTitle(title)
        .setContentText(body)
        .setAutoCancel(true)
        .setContentIntent(contentIntent)
        .build()
    notification.extras.putString("event_id", eventId)
    // Tag plus id 0 matches FCM's android.notification.tag, so this
    // replaces the remote alert for the same room.
    manager.notify(collapseId, 0, notification)
}

private fun canPostNotifications(context: Context): Boolean {
    if (Build.VERSION.SDK_INT < 33) {
        return true
    }
    return context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
        android.content.pm.PackageManager.PERMISSION_GRANTED
}

private fun ensureChannel(manager: NotificationManager) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
        return
    }
    if (manager.getNotificationChannel(CHANNEL_ID) != null) {
        return
    }
    val channel = NotificationChannel(
        CHANNEL_ID,
        "Messages",
        NotificationManager.IMPORTANCE_HIGH,
    )
    channel.description = "New messages"
    manager.createNotificationChannel(channel)
}

private fun smallIcon(context: Context): Int {
    val icon = context.applicationInfo.icon
    return if (icon == 0) android.R.drawable.ic_dialog_info else icon
}

private fun collapseTag(roomId: String, hinted: String?): String {
    if (hinted != null && COLLAPSE_ID.matches(hinted)) {
        return hinted
    }
    val digest = MessageDigest.getInstance("SHA-256").digest(roomId.toByteArray(Charsets.UTF_8))
    return digest.joinToString("") { "%02x".format(it) }.take(32)
}

private fun firstText(primary: String?, fallback: String?): String {
    val first = primary?.trim().orEmpty()
    if (first.isNotEmpty()) {
        return first
    }
    return fallback?.trim().orEmpty()
}

private fun wakeFrom(intent: Intent?): JSObject? {
    if (intent == null) {
        return null
    }
    val account = intent.getStringExtra("account_id")?.trim().orEmpty()
    val room = intent.getStringExtra("room_id")?.trim().orEmpty()
    val event = intent.getStringExtra("event_id")?.trim().orEmpty()
    if (account.isEmpty() || room.isEmpty() || event.isEmpty()) {
        return null
    }
    val wake = JSObject()
    wake.put("account_id", account)
    wake.put("room_id", room)
    wake.put("event_id", event)
    val thread = intent.getStringExtra("thread_root_id")?.trim().orEmpty()
    if (thread.isNotEmpty()) {
        wake.put("thread_root_id", thread)
    }
    return wake
}
