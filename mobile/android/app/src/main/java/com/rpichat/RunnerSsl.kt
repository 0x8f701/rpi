package com.rpichat

import android.content.Context
import java.security.KeyStore
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.net.Socket
import java.util.concurrent.TimeUnit.SECONDS
import javax.net.ssl.HttpsURLConnection
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLEngine
import javax.net.ssl.SSLSession
import javax.net.ssl.TrustManagerFactory
import javax.net.ssl.X509ExtendedTrustManager
import okhttp3.OkHttpClient

fun sha256Hex(cert: X509Certificate): String {
  val digest = MessageDigest.getInstance("SHA-256").digest(cert.encoded)
  return digest.joinToString("") { byte -> "%02x".format(byte) }
}

object RunnerSsl {
  fun applyTo(builder: OkHttpClient.Builder, context: Context) {
    val app = context.applicationContext
    val manager = PinnedTrustManager(app)
    val ssl = SSLContext.getInstance("TLS")
    ssl.init(null, arrayOf(manager), SecureRandom())
    builder.sslSocketFactory(ssl.socketFactory, manager)
    builder.hostnameVerifier(PinnedHostnameVerifier(app))
    builder.connectTimeout(10, SECONDS)
    builder.callTimeout(20, SECONDS)
  }
}

private fun systemTrustManager(): X509ExtendedTrustManager {
  val factory = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm())
  factory.init(null as KeyStore?)
  for (manager in factory.trustManagers) {
    if (manager is X509ExtendedTrustManager) return manager
  }
  throw IllegalStateException("No X509ExtendedTrustManager")
}

private class PinnedTrustManager(private val context: Context) : X509ExtendedTrustManager() {
  private val system = systemTrustManager()

  override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {
    system.checkClientTrusted(chain, authType)
  }

  override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String, socket: Socket) {
    system.checkClientTrusted(chain, authType, socket)
  }

  override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String, engine: SSLEngine) {
    system.checkClientTrusted(chain, authType, engine)
  }

  override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
    try {
      system.checkServerTrusted(chain, authType)
    } catch (error: CertificateException) {
      acceptPinned(chain, null, error)
    }
  }

  override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String, socket: Socket) {
    try {
      system.checkServerTrusted(chain, authType, socket)
    } catch (error: CertificateException) {
      // Reading socket.session here can deadlock the in-progress TLS handshake.
      acceptPinned(chain, null, error)
    }
  }

  override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String, engine: SSLEngine) {
    try {
      system.checkServerTrusted(chain, authType, engine)
    } catch (error: CertificateException) {
      acceptPinned(chain, engine.peerHost, error)
    }
  }

  override fun getAcceptedIssuers(): Array<X509Certificate> = system.acceptedIssuers

  private fun acceptPinned(chain: Array<X509Certificate>, host: String?, cause: CertificateException) {
    if (chain.isEmpty()) throw cause
    val digest = sha256Hex(chain[0])
    if (!host.isNullOrEmpty() && TrustedPins.matches(context, host, digest)) return
    if (TrustedPins.containsDigest(context, digest)) return
    throw cause
  }
}

private class PinnedHostnameVerifier(private val context: Context) : HostnameVerifier {
  private val system: HostnameVerifier = HttpsURLConnection.getDefaultHostnameVerifier()

  override fun verify(hostname: String, session: SSLSession): Boolean {
    if (system.verify(hostname, session)) return true
    val certs = session.peerCertificates
    if (certs.isEmpty()) return false
    val leaf = certs[0] as? X509Certificate ?: return false
    val digest = sha256Hex(leaf)
    return TrustedPins.matches(context, hostname, digest) || TrustedPins.containsDigest(context, digest)
  }
}
