package com.rpichat

import android.content.Context

object TrustedPins {
  private const val PREFS = "rpi_tls_pins_v1"

  fun get(context: Context, host: String): String? {
    return prefs(context).getString(host.lowercase(), null)
  }

  fun put(context: Context, host: String, sha256: String) {
    if (!prefs(context).edit().putString(host.lowercase(), sha256.lowercase()).commit()) {
      throw IllegalStateException("Certificate pin was not saved.")
    }
  }

  fun matches(context: Context, host: String, sha256: String): Boolean {
    val stored = get(context, host) ?: return false
    return stored.equals(sha256, ignoreCase = true)
  }

  fun containsDigest(context: Context, sha256: String): Boolean {
    return prefs(context).all.values.any { it is String && it.equals(sha256, ignoreCase = true) }
  }

  private fun prefs(context: Context) =
    context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
}
