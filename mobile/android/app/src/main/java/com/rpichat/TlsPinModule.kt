package com.rpichat

import android.net.Uri
import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.facebook.react.bridge.WritableMap
import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.util.concurrent.Executors
import javax.net.ssl.SNIHostName
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLSocket
import javax.net.ssl.X509TrustManager

class TlsPinModule(private val ctx: ReactApplicationContext) : ReactContextBaseJavaModule(ctx) {
  private val inspectPool = Executors.newCachedThreadPool()

  override fun getName(): String = "TlsPin"

  @ReactMethod
  fun inspect(origin: String, promise: Promise) {
    inspectPool.execute {
      try {
        promise.resolve(inspectOrigin(origin))
      } catch (error: Exception) {
        promise.reject("tls_inspect", error.message ?: "TLS inspect failed", error)
      }
    }
  }

  @ReactMethod
  fun trust(origin: String, sha256: String, promise: Promise) {
    try {
      val host = originHost(origin)
      val digest = sha256.lowercase()
      if (!SHA256_HEX.matches(digest)) {
        promise.reject("tls_trust", "Certificate fingerprint is not a SHA-256 hex digest.")
        return
      }
      TrustedPins.put(ctx, host, digest)
      promise.resolve(null)
    } catch (error: Exception) {
      promise.reject("tls_trust", error.message ?: "TLS trust failed", error)
    }
  }

  private fun inspectOrigin(origin: String): WritableMap {
    val parsed = originHostPort(origin)
    val dialHost = networkHost(parsed.host)
    val captured = arrayOfNulls<X509Certificate>(1)
    val capturing = object : X509TrustManager {
      override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {
        throw CertificateException("inspect does not accept client certificates")
      }
      override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
        if (chain.isNotEmpty()) captured[0] = chain[0]
      }
      override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }
    val ssl = SSLContext.getInstance("TLS")
    ssl.init(null, arrayOf(capturing), SecureRandom())
    val socket = ssl.socketFactory.createSocket(dialHost, parsed.port) as SSLSocket
    try {
      socket.soTimeout = 8_000
      // SNI carries a DNS name only: a numeric host is dialled without one, and
      // an absolute name is sent without its trailing root label dot.
      if (!isIpLiteral(dialHost)) {
        val params = socket.sslParameters
        params.serverNames = listOf(SNIHostName(dialHost.removeSuffix(".")))
        socket.sslParameters = params
      }
      socket.startHandshake()
    } finally {
      socket.close()
    }
    val cert = captured[0] ?: throw IllegalStateException("Peer presented no certificate.")
    val map = Arguments.createMap()
    map.putString("host", parsed.host)
    map.putString("sha256", sha256Hex(cert))
    map.putString("subject", cert.subjectX500Principal.name)
    map.putString("issuer", cert.issuerX500Principal.name)
    map.putDouble("notAfterMs", cert.notAfter.time.toDouble())
    return map
  }

  companion object {
    private val SHA256_HEX = Regex("^[0-9a-f]{64}$")

    fun originHost(origin: String): String = originHostPort(origin).host

    fun originHostPort(origin: String): HostPort {
      val uri = Uri.parse(origin)
      val scheme = uri.scheme?.lowercase()
      if (scheme != "https" && scheme != "wss") {
        throw IllegalArgumentException("Trust is only for HTTPS runners.")
      }
      val host = uri.host?.lowercase()?.trim().orEmpty()
      if (host.isEmpty()) throw IllegalArgumentException("Origin has no host.")
      val port = when {
        uri.port != -1 -> uri.port
        else -> 443
      }
      return HostPort(host, port)
    }
  }

  data class HostPort(val host: String, val port: Int)
}

/** The dial target for a parsed host: `Uri` keeps the brackets of an IPv6
 *  literal — and the trust key built from the same host keeps them — but they
 *  are not part of the address, so the socket is dialled with the bare form. */
private fun networkHost(host: String): String =
  if (host.startsWith("[") && host.endsWith("]")) host.substring(1, host.length - 1) else host

/** Whether a host names an address rather than a DNS name. SNI carries a DNS
 *  name, so a numeric host is dialled without one: brackets mark an IPv6
 *  literal, a colon cannot appear in a DNS name, and the rest is a dotted
 *  quad. */
private fun isIpLiteral(host: String): Boolean =
  host.startsWith("[") || host.contains(':') || isIpv4Literal(host)

/** Four decimal parts, 0-255. */
private fun isIpv4Literal(host: String): Boolean {
  val parts = host.split('.')
  return parts.size == 4 && parts.all { part ->
    part.length in 1..3 && part.all { it in '0'..'9' } && part.toInt() <= 255
  }
}
