package org.threnody.app

import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertisingSet
import android.bluetooth.le.AdvertisingSetCallback
import android.bluetooth.le.AdvertisingSetParameters
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import android.os.Looper
import android.os.ParcelUuid
import java.util.UUID
import java.util.concurrent.Executors
import uniffi.threnody_ffi.ByteLink
import uniffi.threnody_ffi.ThrenodyNode

/**
 * Bluetooth LE transport (Appendix K), process-wide so it keeps working
 * under [ThrenodyService] with no Activity:
 *
 * - listens on an insecure L2CAP channel and advertises its PSM inside a
 *   private beacon (an extended advert, refreshed every minute);
 * - scans continuously and dials approved contacts it recognises;
 * - offers a one-off scan of every Threnody advert for manual connects.
 */
@Suppress("MissingPermission")
object Bluetooth {
    /** Must match `threnody_net::discovery::BLE_SERVICE_UUID`. */
    const val SERVICE = "7e9f0e1c-3b5a-4c7e-9d2a-5f1e8b6c4a01"
    private val uuid = ParcelUuid(UUID.fromString(SERVICE))
    private const val REFRESH_MS = 60_000L
    /** Must match `threnody-cli`'s `ble::MAX_SDU`. */
    private const val MAX_SDU = 4096

    private val main = Handler(Looper.getMainLooper())
    private val dialer = Executors.newSingleThreadExecutor()
    @Volatile private var server: BluetoothServerSocket? = null
    private var advertising: AdvertisingSet? = null

    val running get() = server != null

    fun permitted(ctx: Context) = permissions.all { granted(ctx, it) }

    /** Enough to listen and advertise; scanning additionally needs BLUETOOTH_SCAN. */
    fun canListen(ctx: Context) =
        granted(ctx, Manifest.permission.BLUETOOTH_CONNECT) && granted(ctx, Manifest.permission.BLUETOOTH_ADVERTISE)

    private fun granted(ctx: Context, p: String) = ctx.checkSelfPermission(p) == PackageManager.PERMISSION_GRANTED

    val permissions = arrayOf(
        Manifest.permission.BLUETOOTH_CONNECT,
        Manifest.permission.BLUETOOTH_ADVERTISE,
        Manifest.permission.BLUETOOTH_SCAN,
    )

    private fun adapter(ctx: Context): BluetoothAdapter? =
        ctx.getSystemService(BluetoothManager::class.java)?.adapter

    /** Starts listening, advertising and auto-dialing (idempotent). */
    @Synchronized
    fun start(ctx: Context, node: ThrenodyNode) {
        if (server != null) return
        if (!canListen(ctx)) return Threnody.say("! Bluetooth needs permission; tap Start Bluetooth")
        val adapter = adapter(ctx) ?: return Threnody.say("! no Bluetooth adapter")
        val s = try { adapter.listenUsingInsecureL2capChannel() }
            catch (e: Exception) { return Threnody.say("! L2CAP listen: ${e.message}") }
        server = s
        Thread {
            while (true) {
                val sock = try { s.accept() } catch (e: Exception) { break }
                Threnody.say("* Bluetooth link from ${sock.remoteDevice.address}")
                attach(node, sock, outbound = false, expect = null)
            }
        }.apply { isDaemon = true }.start()
        main.post { advertise(adapter, node, s.psm) }
        if (granted(ctx, Manifest.permission.BLUETOOTH_SCAN)) main.post { watch(adapter, node) }
        else Threnody.say("* Bluetooth: not scanning (no permission); contacts can still reach us")
    }

    private fun advertise(adapter: BluetoothAdapter, node: ThrenodyNode, psm: Int) {
        val advertiser = adapter.bluetoothLeAdvertiser ?: return Threnody.say("! BLE advertising not supported")
        if (!adapter.isLeExtendedAdvertisingSupported) return Threnody.say("! no BLE extended advertising")
        val params = AdvertisingSetParameters.Builder()
            .setLegacyMode(false) // the beacon needs more than 31 bytes
            .setConnectable(true)
            .setScannable(false)
            .setInterval(AdvertisingSetParameters.INTERVAL_LOW)
            .setTxPowerLevel(AdvertisingSetParameters.TX_POWER_HIGH)
            .build()
        fun data() = AdvertiseData.Builder()
            .addServiceData(uuid, node.bleBeacon(psm.toUShort()))
            .setIncludeDeviceName(false)
            .build()
        val refresh = object : Runnable {
            override fun run() {
                advertising?.setAdvertisingData(data())
                main.postDelayed(this, REFRESH_MS)
            }
        }
        advertiser.startAdvertisingSet(params, data(), null, null, null, object : AdvertisingSetCallback() {
            override fun onAdvertisingSetStarted(set: AdvertisingSet?, txPower: Int, status: Int) {
                if (status != ADVERTISE_SUCCESS) return Threnody.say("! Bluetooth advertise failed ($status)")
                advertising = set
                Threnody.say("* Bluetooth: advertising, L2CAP psm $psm")
                main.postDelayed(refresh, REFRESH_MS)
            }
        })
    }

    private fun settings() = ScanSettings.Builder()
        .setLegacy(false)
        .setPhy(ScanSettings.PHY_LE_ALL_SUPPORTED)
        .setScanMode(ScanSettings.SCAN_MODE_BALANCED)
        .build()

    private fun filter() = ScanFilter.Builder().setServiceData(uuid, ByteArray(0), ByteArray(0)).build()

    /** Scans for as long as the process lives, dialing recognised contacts. */
    private fun watch(adapter: BluetoothAdapter, node: ThrenodyNode) {
        val scanner = adapter.bluetoothLeScanner ?: return Threnody.say("! no Bluetooth scanner")
        scanner.startScan(listOf(filter()), settings(), object : ScanCallback() {
            override fun onScanResult(type: Int, r: ScanResult) {
                val data = r.scanRecord?.getServiceData(uuid) ?: return
                val dial = node.bleHeard(data) ?: return
                Threnody.say("* heard ${Threnody.short(dial.peer)} over Bluetooth; connecting")
                dialer.execute { dial(node, r.device, dial.psm.toInt(), dial.peer) }
            }
            override fun onScanFailed(code: Int) = Threnody.say("! Bluetooth scan failed ($code)")
        })
    }

    /** Lists every Threnody advert heard for 8 seconds (for manual connects). */
    fun scanOnce(ctx: Context, node: ThrenodyNode, done: (List<Pair<BluetoothDevice, Int>>) -> Unit) {
        val scanner = adapter(ctx)?.bluetoothLeScanner ?: return Threnody.say("! no Bluetooth scanner")
        val found = LinkedHashMap<String, Pair<BluetoothDevice, Int>>()
        val cb = object : ScanCallback() {
            override fun onScanResult(type: Int, r: ScanResult) {
                val data = r.scanRecord?.getServiceData(uuid) ?: return
                val psm = node.bleAdvertPsm(data) ?: return
                found[r.device.address] = r.device to psm.toInt()
            }
        }
        val fast = ScanSettings.Builder()
            .setLegacy(false)
            .setPhy(ScanSettings.PHY_LE_ALL_SUPPORTED)
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .build()
        scanner.startScan(listOf(filter()), fast, cb)
        main.postDelayed({
            scanner.stopScan(cb)
            done(found.values.toList())
        }, 8000)
    }

    /**
     * Opens an L2CAP channel and starts a session, pinning `expect` if
     * given. LE connection setup fails transiently (HCI 0x3e) often enough
     * that a few attempts are worth making.
     */
    fun dial(node: ThrenodyNode, device: BluetoothDevice, psm: Int, expect: String?) {
        Threnody.say("* dialing ${device.address} psm $psm…")
        for (attempt in 1..3) {
            try {
                val sock = device.createInsecureL2capChannel(psm)
                sock.connect()
                attach(node, sock, outbound = true, expect = expect)
                return
            } catch (e: Exception) {
                if (attempt == 3) Threnody.say("! Bluetooth dial: ${e.message}")
                else Thread.sleep(500)
            }
        }
    }

    /** Bridges one Bluetooth socket into the node. */
    private fun attach(node: ThrenodyNode, sock: BluetoothSocket, outbound: Boolean, expect: String?) {
        val out = sock.outputStream
        // BluetoothSocket truncates L2CAP writes larger than one packet
        // (dropping the rest), and Android stalls on large SDUs, so write
        // pieces of at most one packet and MAX_SDU bytes.
        val packet = minOf(sock.maxTransmitPacketSize.takeIf { it > 0 } ?: 23, MAX_SDU)
        val link = object : ByteLink {
            override fun send(data: ByteArray): Boolean = try {
                var off = 0
                while (off < data.size) {
                    val n = minOf(packet, data.size - off)
                    out.write(data, off, n)
                    off += n
                }
                out.flush()
                true
            } catch (e: Exception) { false }
            override fun disconnect() { try { sock.close() } catch (_: Exception) {} }
        }
        val handle = try {
            node.attachLink(link, outbound, "ble", sock.remoteDevice.address, expect)
        } catch (e: Exception) {
            sock.close()
            return Threnody.say("! Bluetooth attach: ${e.message}")
        }
        Thread {
            // Reads return whole L2CAP SDUs, which can be up to 65535 bytes
            // whatever maxReceivePacketSize says; a smaller buffer stalls.
            val buf = ByteArray(65536)
            try {
                val input = sock.inputStream
                while (true) {
                    val n = input.read(buf)
                    if (n < 0) break
                    handle.receive(buf.copyOf(n))
                }
            } catch (_: Exception) {
            } finally {
                handle.closed()
            }
        }.apply { isDaemon = true }.start()
    }
}
