package to.iris.browser

import android.content.Intent
import android.os.Bundle
import android.util.Log
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  companion object {
    private const val TAG = "IrisMainActivity"
  }

  private var pendingLaunchDeepLink: Intent? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    pendingLaunchDeepLink = sanitizeDeepLinkIntent(intent)
    super.onCreate(savedInstanceState)
  }

  override fun onResume() {
    super.onResume()
    dispatchPendingDeepLink()
  }

  override fun onNewIntent(intent: Intent) {
    val deepLinkIntent = sanitizeDeepLinkIntent(intent)
    super.onNewIntent(intent)
    if (deepLinkIntent != null) {
      pendingLaunchDeepLink = deepLinkIntent
      dispatchPendingDeepLink()
    }
  }

  private fun dispatchPendingDeepLink() {
    val deepLinkIntent = pendingLaunchDeepLink ?: return
    pendingLaunchDeepLink = null
    Log.i(TAG, "Dispatching sanitized deep link ${deepLinkIntent.data}")
    pluginManager.onNewIntent(deepLinkIntent)
  }

  private fun sanitizeDeepLinkIntent(intent: Intent?): Intent? {
    if (intent?.action != Intent.ACTION_VIEW || intent.data == null) {
      return null
    }

    val deepLinkIntent = Intent(intent)
    val sanitizedIntent = Intent(intent).apply {
      action = Intent.ACTION_MAIN
      data = null
      categories?.toList()?.forEach { removeCategory(it) }
      addCategory(Intent.CATEGORY_LAUNCHER)
    }
    Log.i(TAG, "Sanitizing launch deep link ${deepLinkIntent.data}")
    setIntent(sanitizedIntent)
    return deepLinkIntent
  }
}
