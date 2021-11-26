<svelte:head>
  <script src="https://cdn.plot.ly/plotly-latest.min.js" type="text/javascript"></script>
</svelte:head>

<script lang="ts">
  import Home from '@/pages/Home.svelte'
  import Header from '@/components/Header.svelte'
  import type { ConsumerType } from '@/types/app'
  import { WebSocketStatus, MitigationMode } from '@/types/app'
  import { onMount, onDestroy } from 'svelte';

  let ws: WebSocket | undefined = undefined
  let websocketStatus: WebSocketStatus = WebSocketStatus.Connecting
  let consumers: Map<string, ConsumerType> = new Map ()
  let consumers_array: Array<ConsumerType> = []
  let timeout: ReturnType<typeof setTimeout> | undefined = undefined

  const updateConsumerStats = (consumer: ConsumerType, stats: Object) => {
    if (stats["consumer-stats"]["video-encoders"].length > 0) {
      let venc = stats["consumer-stats"]["video-encoders"][0]
      consumer.stats["target_bitrate"] = venc["bitrate"]
      consumer.video_codec = venc["codec-name"]

      let mitigation_mode = MitigationMode.None

      for (let mode of venc["mitigation-mode"].split("+")) {
        switch (mode) {
          case "none": {
            mitigation_mode |= MitigationMode.None
            break
          }
          case "downscaled": {
            mitigation_mode |= MitigationMode.Downscaled
            break
          }
          case "downsampled": {
            mitigation_mode |= MitigationMode.Downsampled
            break
          } 
        }
      }

      consumer.mitigation_mode = mitigation_mode
    } else {
      consumer.stats["target_bitrate"] = 0
    }

    for (let svalue of Object.values(stats)) {
      if (svalue["type"] == "transport") {
        let twcc_stats = svalue["gst-twcc-stats"]
        if (twcc_stats !== undefined) {
          consumer.stats["bitrate_sent"] = twcc_stats["bitrate-sent"]
          consumer.stats["bitrate_recv"] = twcc_stats["bitrate-recv"]
          consumer.stats["packet_loss"] = twcc_stats["packet-loss-pct"]
          consumer.stats["delta_of_delta"] = twcc_stats["avg-delta-of-delta"]
        }
      }
    }

  }

  const fetchStats = () => {
    ws = new WebSocket("ws://127.0.0.1:8484");

    ws.onerror = () => {
      websocketStatus = WebSocketStatus.Error
    }

    ws.onclose = () => {
      websocketStatus = WebSocketStatus.Error
      consumers = new Map()
      consumers_array = []
      timeout = setTimeout(fetchStats, 500)
    }

    ws.onopen = () => {
      websocketStatus = WebSocketStatus.Connected
    }

    ws.onmessage = (event) => {
      let stats = JSON.parse(event.data)
      // Set is supposed to be buildable from an iterator,
      // no idea why the Arra.from is needed ..
      let to_remove = new Set(Array.from(consumers.keys()))

      for (let [key, value] of Object.entries(stats)) {
        let consumer = undefined;

        if (consumers.get(key) === undefined) {
          consumer = {
            id: key,
            video_codec: undefined,
            mitigation_mode: MitigationMode.None,
            stats: new Map([
              ["target_bitrate", 0],
              ["bitrate_sent", 0],
              ["bitrate_recv", 0],
              ["packet_loss", 0],
              ["delta_of_delta", 0]
            ]),
          }
          consumers.set(key, consumer)
        } else {
          consumer = consumers.get(key)
        }

        updateConsumerStats(consumer, value)

        to_remove.delete(key)
      }

      for (let key of to_remove) {
        consumers.delete(key)
      }

      consumers_array = Array.from(consumers.values())
    }
  }

  const closeWebSocket = () => {
    if (ws != undefined) {
      ws.close();
      ws = undefined;
    }

    if (timeout != undefined) {
      clearTimeout(timeout)
      timeout = undefined
    }
  }

  onMount(fetchStats)
  onDestroy(closeWebSocket)
</script>

<Header websocketStatus={ websocketStatus } />

<Home consumers={ consumers_array } />

<style lang="scss">
  :root {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen,
      Ubuntu, Cantarell, 'Open Sans', 'Helvetica Neue', sans-serif;
    height: 100%;
  }
  :global(body) {
    /* this will apply to <body> */
    margin: 0;
    height: 100%;
    background-color: #fbfbfb;
  }
</style>
