package to.iris.browser.mobile

import android.app.Activity
import android.graphics.Bitmap
import android.graphics.Color
import android.net.http.SslError
import android.net.Uri
import android.util.Log
import android.util.TypedValue
import android.view.ViewGroup
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.SslErrorHandler
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.FrameLayout
import androidx.core.content.ContextCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import kotlin.math.roundToInt

@InvokeArg
class CreateArgs {
    lateinit var label: String
    lateinit var url: String
    var x: Double = 0.0
    var y: Double = 0.0
    var width: Double = 0.0
    var height: Double = 0.0
    var scale: Double = 1.0
    lateinit var initScript: String
    lateinit var diagnosticScript: String
    var allowedOriginRule: String? = null
    var actualUrlRoot: String? = null
}

@InvokeArg
class LabelArgs {
    lateinit var label: String
}

@InvokeArg
class NavigateArgs {
    lateinit var label: String
    lateinit var url: String
}

@InvokeArg
class BoundsArgs {
    lateinit var label: String
    var x: Double = 0.0
    var y: Double = 0.0
    var width: Double = 0.0
    var height: Double = 0.0
    var scale: Double = 1.0
}

@InvokeArg
class ShellOverlayArgs {
    var enabled: Boolean = false
    var x: Double = 0.0
    var y: Double = 0.0
    var width: Double = 0.0
    var height: Double = 0.0
    var scale: Double = 1.0
}

@InvokeArg
class HistoryArgs {
    lateinit var label: String
    lateinit var direction: String
}

private data class BrowserEntry(
    val args: CreateArgs,
    val webView: WebView,
)

@TauriPlugin
class MobileBrowserPlugin(private val activity: Activity) : Plugin(activity) {
    companion object {
        private const val TAG = "MobileBrowserPlugin"
    }

    private val browsers = mutableMapOf<String, BrowserEntry>()

    @Command
    fun create(invoke: Invoke) {
        val args = invoke.parseArgs(CreateArgs::class.java)
        activity.runOnUiThread {
            destroyBrowser(args.label)

            val webView = WebView(activity)
            configureWebView(webView, args)
            browsers[args.label] = BrowserEntry(args, webView)

            val root = contentRoot()
            root.setBackgroundColor(resolveThemeBackgroundColor())
            root.addView(webView, layoutParams(args))
            root.bringChildToFront(webView)
            webView.bringToFront()
            root.requestLayout()
            root.invalidate()
            Log.i(TAG, "create ${args.label} url=${args.url} bounds=${describe(args.x, args.y, args.width, args.height, args.scale)}")
            webView.loadUrl(args.url)
            invoke.resolve()
        }
    }

    @Command
    fun close(invoke: Invoke) {
        val args = invoke.parseArgs(LabelArgs::class.java)
        activity.runOnUiThread {
            destroyBrowser(args.label)
            invoke.resolve()
        }
    }

    @Command
    fun navigate(invoke: Invoke) {
        val args = invoke.parseArgs(NavigateArgs::class.java)
        activity.runOnUiThread {
            val browser = browsers[args.label]?.webView
            if (browser == null) {
                invoke.reject("Webview ${args.label} not found")
                return@runOnUiThread
            }
            Log.i(TAG, "navigate ${args.label} url=${args.url}")
            browser.loadUrl(args.url)
            invoke.resolve()
        }
    }

    @Command
    fun setBounds(invoke: Invoke) {
        val args = invoke.parseArgs(BoundsArgs::class.java)
        activity.runOnUiThread {
            val browser = browsers[args.label]?.webView
            if (browser == null) {
                invoke.reject("Webview ${args.label} not found")
                return@runOnUiThread
            }
            browser.layoutParams = layoutParams(args)
            contentRoot().bringChildToFront(browser)
            browser.bringToFront()
            browser.requestLayout()
            Log.i(TAG, "setBounds ${args.label} bounds=${describe(args.x, args.y, args.width, args.height, args.scale)}")
            invoke.resolve()
        }
    }

    @Command
    fun setShellOverlay(invoke: Invoke) {
        invoke.parseArgs(ShellOverlayArgs::class.java)
        activity.runOnUiThread {
            invoke.resolve()
        }
    }

    @Command
    fun history(invoke: Invoke) {
        val args = invoke.parseArgs(HistoryArgs::class.java)
        activity.runOnUiThread {
            val browser = browsers[args.label]?.webView
            if (browser == null) {
                invoke.reject("Webview ${args.label} not found")
                return@runOnUiThread
            }
            when (args.direction) {
                "back" -> if (browser.canGoBack()) browser.goBack()
                "forward" -> if (browser.canGoForward()) browser.goForward()
                else -> {
                    invoke.reject("Invalid history direction")
                    return@runOnUiThread
                }
            }
            invoke.resolve()
        }
    }

    @Command
    fun reload(invoke: Invoke) {
        val args = invoke.parseArgs(LabelArgs::class.java)
        activity.runOnUiThread {
            val browser = browsers[args.label]?.webView
            if (browser == null) {
                invoke.reject("Webview ${args.label} not found")
                return@runOnUiThread
            }
            browser.reload()
            invoke.resolve()
        }
    }

    @Command
    fun currentUrl(invoke: Invoke) {
        val args = invoke.parseArgs(LabelArgs::class.java)
        activity.runOnUiThread {
            val browser = browsers[args.label]?.webView
            if (browser == null) {
                invoke.reject("Webview ${args.label} not found")
                return@runOnUiThread
            }
            val payload = JSObject()
            payload.put("url", browser.url)
            invoke.resolve(payload)
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        activity.runOnUiThread {
            browsers.keys.toList().forEach(::destroyBrowser)
        }
    }

    private fun configureWebView(webView: WebView, args: CreateArgs) {
        with(webView.settings) {
            javaScriptEnabled = true
            domStorageEnabled = true
            mediaPlaybackRequiresUserGesture = false
            javaScriptCanOpenWindowsAutomatically = true
            mixedContentMode = WebSettings.MIXED_CONTENT_ALWAYS_ALLOW
        }

        webView.setBackgroundColor(resolveThemeBackgroundColor())
        webView.webChromeClient = WebChromeClient()

        if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
            val allowedOrigins = setOf(args.allowedOriginRule ?: "*")
            WebViewCompat.addDocumentStartJavaScript(webView, args.initScript, allowedOrigins)
        }

        webView.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                if (!request.isForMainFrame) return false
                val url = request.url.toString()
                return shouldInterceptNavigation(args, url)
            }

            override fun onPageStarted(view: WebView, url: String?, favicon: Bitmap?) {
                super.onPageStarted(view, url, favicon)
                val safeUrl = url ?: return
                Log.i(TAG, "page started ${args.label} url=$safeUrl")
                emitLocation(args.label, safeUrl, "navigation")
                emitPageLoad(args.label, safeUrl, "started")
            }

            override fun onPageFinished(view: WebView, url: String?) {
                super.onPageFinished(view, url)
                val safeUrl = url ?: return
                Log.i(TAG, "page finished ${args.label} url=$safeUrl")
                if (shouldInjectScripts(args, safeUrl)) {
                    injectScripts(view, args)
                }
                emitPageLoad(args.label, safeUrl, "finished")
            }

            override fun onReceivedError(
                view: WebView,
                request: WebResourceRequest,
                error: WebResourceError,
            ) {
                super.onReceivedError(view, request, error)
                if (!request.isForMainFrame) return
                val failingUrl = request.url.toString()
                val description = error.description?.toString() ?: "Failed to load page"
                Log.e(TAG, "page error ${args.label} url=$failingUrl code=${error.errorCode} error=$description")
                emitDiagnostic(args.label, failingUrl, "page-load-error", description)
            }

            override fun onReceivedSslError(
                view: WebView,
                handler: SslErrorHandler,
                error: SslError,
            ) {
                super.onReceivedSslError(view, handler, error)
                val failingUrl = error.url ?: view.url ?: args.url
                val description = "TLS error ${error.primaryError}"
                Log.e(TAG, "ssl error ${args.label} url=$failingUrl error=$description")
                emitDiagnostic(args.label, failingUrl, "ssl-error", description)
            }
        }
    }

    private fun shouldInterceptNavigation(args: CreateArgs, url: String): Boolean {
        if (url.startsWith("htree://")) {
            emitLocation(args.label, url, "navigation")
            return true
        }

        if (args.actualUrlRoot != null) {
            if (!url.startsWith(args.actualUrlRoot!!)) {
                emitLocation(args.label, url, "navigation")
                return true
            }
            return false
        }

        val allowedOrigin = args.allowedOriginRule ?: return false
        if (originOf(url) != allowedOrigin) {
            emitLocation(args.label, url, "navigation")
            return true
        }

        return false
    }

    private fun shouldInjectScripts(args: CreateArgs, url: String): Boolean {
        if (args.actualUrlRoot != null) {
            return url.startsWith(args.actualUrlRoot!!)
        }

        val allowedOrigin = args.allowedOriginRule ?: return true
        return originOf(url) == allowedOrigin
    }

    private fun injectScripts(webView: WebView, args: CreateArgs) {
        webView.evaluateJavascript(args.initScript, null)
        webView.evaluateJavascript(args.diagnosticScript, null)
        webView.postDelayed({ webView.evaluateJavascript(args.initScript, null) }, 150)
        webView.postDelayed({ webView.evaluateJavascript(args.initScript, null) }, 1000)
        webView.postDelayed({ webView.evaluateJavascript(args.diagnosticScript, null) }, 150)
        webView.postDelayed({ webView.evaluateJavascript(args.diagnosticScript, null) }, 1000)
    }

    private fun emitLocation(label: String, url: String, source: String) {
        val payload = JSObject()
        payload.put("label", label)
        payload.put("url", url)
        payload.put("source", source)
        trigger("location", payload)
    }

    private fun emitPageLoad(label: String, url: String, event: String) {
        val payload = JSObject()
        payload.put("label", label)
        payload.put("url", url)
        payload.put("event", event)
        trigger("page-load", payload)
    }

    private fun emitDiagnostic(label: String, url: String?, source: String, error: String) {
        val payload = JSObject()
        payload.put("label", label)
        payload.put("url", url)
        payload.put("source", source)
        payload.put("error", error)
        trigger("diagnostic", payload)
    }

    private fun destroyBrowser(label: String) {
        val entry = browsers.remove(label) ?: return
        (entry.webView.parent as? ViewGroup)?.removeView(entry.webView)
        entry.webView.stopLoading()
        entry.webView.webChromeClient = null
        entry.webView.webViewClient = WebViewClient()
        entry.webView.removeAllViews()
        entry.webView.destroy()
    }

    private fun contentRoot(): FrameLayout {
        return activity.findViewById(android.R.id.content)
    }

    private fun layoutParams(args: CreateArgs): FrameLayout.LayoutParams {
        return FrameLayout.LayoutParams(
            nativePx(args.width, args.scale),
            nativePx(args.height, args.scale),
        ).apply {
            leftMargin = nativePx(args.x, args.scale)
            topMargin = nativePx(args.y, args.scale)
        }
    }

    private fun layoutParams(args: BoundsArgs): FrameLayout.LayoutParams {
        return FrameLayout.LayoutParams(
            nativePx(args.width, args.scale),
            nativePx(args.height, args.scale),
        ).apply {
            leftMargin = nativePx(args.x, args.scale)
            topMargin = nativePx(args.y, args.scale)
        }
    }

    private fun nativePx(value: Double, scale: Double): Int {
        val safeScale = if (scale.isFinite() && scale > 0.0) scale else 1.0
        return (value * safeScale).roundToInt().coerceAtLeast(0)
    }

    private fun describe(x: Double, y: Double, width: Double, height: Double, scale: Double): String {
        return "css=($x,$y ${width}x$height) scale=$scale native=(${nativePx(x, scale)},${nativePx(y, scale)} ${nativePx(width, scale)}x${nativePx(height, scale)})"
    }

    private fun originOf(url: String): String? {
        return try {
            val uri = Uri.parse(url)
            val scheme = uri.scheme ?: return null
            val host = uri.host ?: return scheme
            val port = uri.port
            if (port >= 0) "$scheme://$host:$port" else "$scheme://$host"
        } catch (_: Exception) {
            null
        }
    }

    private fun resolveThemeBackgroundColor(): Int {
        val typedValue = TypedValue()
        val resolved = activity.theme.resolveAttribute(android.R.attr.colorBackground, typedValue, true)
        if (!resolved) {
            return Color.WHITE
        }
        return if (typedValue.resourceId != 0) {
            ContextCompat.getColor(activity, typedValue.resourceId)
        } else {
            typedValue.data
        }
    }
}
