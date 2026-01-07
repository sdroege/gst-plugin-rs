import React, { useEffect, useRef, useState } from "react";
import { PlayCircle, StopCircle, Activity } from "lucide-react";

const FRAME_TYPE_KEY = 0x0001;
const FRAME_RATE = 15;

const WebTransportPlayer = () => {
  const [isConnected, setIsConnected] = useState(false);
  const [isPlaying, setIsPlaying] = useState(false);
  const [error, setError] = useState(null);

  const frameDurationUs = useRef(BigInt(Math.floor(1_000_000 / FRAME_RATE)));
  const frameCounterRef = useRef(0n);
  const canvasRef = useRef(null);
  const transportRef = useRef(null);
  const decoderRef = useRef(null);
  const frameCountRef = useRef(0);
  const animationFrameRef = useRef(null);

  const initializeDecoder = () => {
    if (decoderRef.current) {
      decoderRef.current.close();
    }

    const decoder = new VideoDecoder({
      output: (frame) => {
        const canvas = canvasRef.current;
        if (!canvas) {
          frame.close();
          return;
        }

        const ctx = canvas.getContext("2d");
        if (
          canvas.width !== frame.displayWidth ||
          canvas.height !== frame.displayHeight
        ) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }

        ctx.drawImage(frame, 0, 0);
        frame.close();

        frameCountRef.current++;
      },
      error: (e) => {
        console.error("Decoder error:", e);
        setError(`Decoder error: ${e.message}`);
      },
    });

    decoder.configure({
      codec: "avc1.42E016", // H.264 Constrained Baseline Profile
      codedWidth: 640,
      codedHeight: 360,
      optimizeForLatency: true,
    });

    decoderRef.current = decoder;
  };

  const connectWebTransport = async () => {
    try {
      setError(null);

      const transport = new WebTransport("https://localhost:4433");
      await transport.ready;

      transportRef.current = transport;
      setIsConnected(true);
      setIsPlaying(true);

      initializeDecoder();

      // Datagram won't be able to handle the payload sizes we are sending
      const stream = await transport.createBidirectionalStream();
      const reader = stream.readable.getReader();

      const readLoop = async () => {
        try {
          // State for frame assembly
          let pendingFrame = null; // { type: number, expectedSize: number }
          let buffer = new Uint8Array(0);
          let bufferPending = false;
          let pendingBufferSize = 0;
          let pendingFrameType = 0;

          while (true) {
            const { value, done } = await reader.read();
            if (done) {
              console.log("Nothing more to read!");
              break;
            }

            // Handle partial stream reads as QUIC/WebTransport streams do not preserve frame boundaries.
            const newBuffer = new Uint8Array(buffer.length + value.length);
            newBuffer.set(buffer);
            newBuffer.set(value, buffer.length);
            buffer = newBuffer;

            while (true) {
              if (!pendingFrame) {
                // Need to read a new frame header
                if (buffer.length < 6) {
                  // console.log("Not enough data for header, waiting");
                  break;
                }

                // Read frame header
                const view = new DataView(buffer.buffer, buffer.byteOffset, 6);
                const frameType = view.getUint16(0);
                const frameSize = view.getUint32(2);

                // Remove header from buffer
                buffer = buffer.slice(6);

                // Initialize pending frame
                pendingFrame = {
                  type: frameType,
                  expectedSize: frameSize,
                };
              }

              // Check if we have all the data for current pending frame
              if (buffer.length < pendingFrame.expectedSize) {
                // console.log('Incomplete frame - Need:', pendingFrame.expectedSize, 'Have:', buffer.length);
                break; // Wait for more data
              }

              // Extract complete frame data (we have enough now)
              const frameData = buffer.slice(0, pendingFrame.expectedSize);
              const type =
                pendingFrame.type === FRAME_TYPE_KEY ? "key" : "delta";

              // console.log('Complete frame ready - Type:', type, 'Size:', frameData.length);

              if (
                decoderRef.current &&
                decoderRef.current.state === "configured"
              ) {
                const timestamp =
                  frameCounterRef.current * frameDurationUs.current;

                try {
                  const chunk = new EncodedVideoChunk({
                    type,
                    timestamp: Number(timestamp),
                    data: frameData,
                  });

                  decoderRef.current.decode(chunk);
                  frameCounterRef.current++;
                  // console.log('Frame decoded successfully, counter:', frameCounterRef.current);
                } catch (decodeErr) {
                  console.error(
                    "Decode error:",
                    decodeErr,
                    "Frame size:",
                    frameData.length,
                    "Type:",
                    type,
                  );
                }
              }

              // Remove processed frame data from buffer
              buffer = buffer.slice(pendingFrame.expectedSize);

              // Reset pending frame for next frame
              pendingFrame = null;
            }
          }
        } catch (err) {
          if (err.name !== "AbortError") {
            console.error("Read error:", err);
            setError(`Connection error: ${err.message}`);
          }
        }
      };

      readLoop().catch((err) => {
        console.error("Read loop error:", err);
        setError(`Read error: ${err.message}`);
      });

      transport.closed
        .then(() => {
          setIsConnected(false);
          setIsPlaying(false);
        })
        .catch((err) => {
          console.error("Transport closed with error:", err);
          setError(`Transport error: ${err.message}`);
          setIsConnected(false);
          setIsPlaying(false);
        });
    } catch (err) {
      console.error("Connection failed:", err);
      setError(`Failed to connect: ${err.message}`);
      setIsConnected(false);
    }
  };

  const disconnect = () => {
    if (transportRef.current) {
      transportRef.current.close();
      transportRef.current = null;
    }

    if (decoderRef.current) {
      decoderRef.current.close();
      decoderRef.current = null;
    }

    setIsConnected(false);
    setIsPlaying(false);
    frameCounterRef.current = 0n;
  };

  useEffect(() => {
    return () => {
      disconnect();
      if (animationFrameRef.current) {
        cancelAnimationFrame(animationFrameRef.current);
      }
    };
  }, []);

  return (
    <div className="min-h-screen bg-gradient-to-br from-gray-900 to-gray-800 text-white p-8">
      <div className="max-w-6xl mx-auto">
        <div className="mb-8">
          <h1 className="text-4xl font-bold mb-2">WebTransport H.264 Player</h1>
          <p className="text-gray-400">
            Streaming video via WebTransport from GStreamer pipeline
          </p>
        </div>

        {error && (
          <div className="mb-6 bg-red-900/50 border border-red-700 rounded-lg p-4">
            <p className="text-red-200">{error}</p>
          </div>
        )}

        <div className="bg-gray-800 rounded-lg shadow-2xl overflow-hidden mb-6">
          <div className="aspect-video bg-black flex items-center justify-center">
            <canvas
              ref={canvasRef}
              className="max-w-full max-h-full"
              style={{ imageRendering: "auto" }}
            />
            {!isPlaying && (
              <div className="absolute text-gray-500">
                <Activity size={64} className="animate-pulse" />
              </div>
            )}
          </div>
        </div>

        <div className="grid grid-cols-1 md:grid-cols-2 gap-6 mb-6">
          <div className="bg-gray-800 rounded-lg p-6">
            <h2 className="text-xl font-semibold mb-4">Connection</h2>
            <div className="space-y-3">
              <div className="flex items-center justify-between">
                <span className="text-gray-400">Status</span>
                <span
                  className={`px-3 py-1 rounded-full text-sm ${
                    isConnected
                      ? "bg-green-900/50 text-green-300"
                      : "bg-gray-700 text-gray-400"
                  }`}
                >
                  {isConnected ? "Connected" : "Disconnected"}
                </span>
              </div>
              <div className="flex items-center justify-between">
                <span className="text-gray-400">Server</span>
                <span className="text-white">localhost:4433</span>
              </div>
            </div>

            <div className="mt-6">
              {!isConnected ? (
                <button
                  onClick={connectWebTransport}
                  className="w-full bg-blue-600 hover:bg-blue-700 text-white py-3 rounded-lg flex items-center justify-center gap-2 transition-colors"
                >
                  <PlayCircle size={20} />
                  Connect & Play
                </button>
              ) : (
                <button
                  onClick={disconnect}
                  className="w-full bg-red-600 hover:bg-red-700 text-white py-3 rounded-lg flex items-center justify-center gap-2 transition-colors"
                >
                  <StopCircle size={20} />
                  Disconnect
                </button>
              )}
            </div>
          </div>
        </div>

        <div className="bg-gray-800 rounded-lg p-6">
          <h2 className="text-xl font-semibold mb-4">Requirements</h2>
          <ul className="space-y-2 text-gray-300">
            <li className="flex items-start gap-2">
              <span className="text-green-400 mt-1">•</span>
              <span>
                Chrome/Edge browser (WebTransport & WebCodecs support required)
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span className="text-green-400 mt-1">•</span>
              <span>
                GStreamer pipeline running with quinnwtsink on localhost:4433
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span className="text-green-400 mt-1">•</span>
              <span>
                Valid TLS certificate for localhost (see setup instructions)
              </span>
            </li>
          </ul>
        </div>
      </div>
    </div>
  );
};

export default WebTransportPlayer;
