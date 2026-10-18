/**
 * adsum Client - Vencord/Userscript Plugin
 *
 * Drop-in replacement for arRPC WebRichPresence plugin with optional
 * MessagePack support for ~16% smaller messages.
 *
 * The adsum bridge exposes two ports:
 *   - 1337 (JSON) and 1338 (MessagePack), or any ws_port pair.
 * The server auto-detects the protocol, so the client can also connect to
 * the JSON port with `?format=json` and skip MessagePack entirely.
 *
 * Usage in browser console or userscript:
 * ```javascript
 * const client = new AdsumClient(true); // true = use MessagePack
 * client.onActivity = (activity) => {
 *   // Handle activity update
 *   console.log('Activity:', activity);
 * };
 * client.connect();
 * ```
 *
 * For MessagePack support, include @msgpack/msgpack:
 * <script src="https://unpkg.com/@msgpack/msgpack"></script>
 */

class AdsumClient {
    /**
     * Create a new adsum client
     * @param {boolean} useMsgPack - Use MessagePack protocol (default: false for arRPC compat)
     * @param {object} options - Additional options
     * @param {number} options.jsonPort - JSON bridge port (default: 1337)
     * @param {number} options.msgpackPort - MessagePack bridge port (default: 1338)
     * @param {number} options.reconnectInterval - Reconnect interval in ms (default: 5000)
     */
    constructor(useMsgPack = false, options = {}) {
        this.useMsgPack = useMsgPack && typeof MessagePack !== 'undefined';
        this.jsonPort = options.jsonPort || 1337;
        this.msgpackPort = options.msgpackPort || 1338;
        this.reconnectInterval = options.reconnectInterval || 5000;

        this.ws = null;
        this.connected = false;
        this.reconnectTimer = null;

        // Callbacks
        this.onActivity = null;
        this.onConnect = null;
        this.onDisconnect = null;
        this.onError = null;
    }

    /**
     * Get the connection port based on protocol
     */
    get port() {
        return this.useMsgPack ? this.msgpackPort : this.jsonPort;
    }

    /**
     * Get the protocol format string
     */
    get format() {
        return this.useMsgPack ? 'msgpack' : 'json';
    }

    /**
     * Connect to adsum server
     */
    connect() {
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
            console.log('[adsum] Already connected');
            return;
        }

        const url = `ws://127.0.0.1:${this.port}?format=${this.format}`;
        console.log(`[adsum] Connecting to ${url}`);

        try {
            this.ws = new WebSocket(url);
            this.ws.binaryType = 'arraybuffer';

            this.ws.onopen = () => {
                console.log('[adsum] Connected');
                this.connected = true;
                if (this.onConnect) this.onConnect();
            };

            this.ws.onclose = (event) => {
                console.log('[adsum] Disconnected:', event.code, event.reason);
                this.connected = false;
                if (this.onDisconnect) this.onDisconnect();
                this.scheduleReconnect();
            };

            this.ws.onerror = (error) => {
                console.error('[adsum] Error:', error);
                if (this.onError) this.onError(error);
            };

            this.ws.onmessage = (event) => {
                try {
                    const data = this.decode(event.data);
                    if (this.onActivity) this.onActivity(data);
                } catch (e) {
                    console.error('[adsum] Failed to decode message:', e);
                }
            };
        } catch (e) {
            console.error('[adsum] Failed to connect:', e);
            this.scheduleReconnect();
        }
    }

    /**
     * Disconnect from adsum server
     */
    disconnect() {
        if (this.reconnectTimer) {
            clearTimeout(this.reconnectTimer);
            this.reconnectTimer = null;
        }

        if (this.ws) {
            this.ws.close();
            this.ws = null;
        }

        this.connected = false;
    }

    /**
     * Decode incoming message
     * @private
     */
    decode(data) {
        if (this.useMsgPack && data instanceof ArrayBuffer) {
            return MessagePack.decode(new Uint8Array(data));
        } else if (typeof data === 'string') {
            return JSON.parse(data);
        } else {
            // Try to decode as string
            const text = new TextDecoder().decode(data);
            return JSON.parse(text);
        }
    }

    /**
     * Schedule reconnection attempt
     * @private
     */
    scheduleReconnect() {
        if (this.reconnectTimer) return;

        console.log(`[adsum] Reconnecting in ${this.reconnectInterval}ms`);
        this.reconnectTimer = setTimeout(() => {
            this.reconnectTimer = null;
            this.connect();
        }, this.reconnectInterval);
    }
}

// Vencord plugin export (for use as Vencord plugin)
if (typeof Vencord !== 'undefined') {
    Vencord.Plugins.registerPlugin({
        name: 'Adsum',
        description: 'High-performance Discord RPC bridge with optional MessagePack',
        authors: [{ name: 'spikehd' }],

        settings: {
            useMsgPack: {
                type: 'boolean',
                default: false,
                description: '[EXPERIMENTAL] Use MessagePack for smaller, faster messages',
            },
        },

        start() {
            this.client = new AdsumClient(this.settings.useMsgPack);
            this.client.onActivity = (data) => {
                // Forward to Discord's presence system
                if (data.activity) {
                    Vencord.Webpack.findByProps('getLocalPresence')?.setLocalPresence?.(data);
                }
            };
            this.client.connect();
        },

        stop() {
            if (this.client) {
                this.client.disconnect();
                this.client = null;
            }
        },
    });
}

// Export for Node.js / bundlers
if (typeof module !== 'undefined' && module.exports) {
    module.exports = { AdsumClient };
}

// Export for ES modules
if (typeof window !== 'undefined') {
    window.AdsumClient = AdsumClient;
}