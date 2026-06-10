import subprocess
import threading
import sys
import time

sync_log = open("multiverse_sync.log", "w")
sync_lock = threading.Lock()

def monitor_output(process, p_id):
    """Reads stdout from a child process and filters heartbeats/desyncs."""
    with open(f"p{p_id}_full.log", "w") as full_log:
        for line in iter(process.stdout.readline, ''):
            full_log.write(line)
            full_log.flush()

            # Filter for Heartbeats or Fatal errors
            if "[HEARTBEAT]" in line or "FATAL" in line or "Divergence" in line:
                with sync_lock:
                    formatted_line = f"[P{p_id}] {line}"
                    sys.stdout.write(formatted_line)
                    sync_log.write(formatted_line)
                    sync_log.flush()

def main():
    print("===========================================")
    print(" IGNITING V2 LOCAL MULTIVERSE (8 NODES)")
    print("===========================================")

    nodes = []

    # Rapid-fire 8 local nodes, assigning them IDs 0 through 7
    for i in range(8):
        print(f"[HARNESS] Booting Node {i} on Port {50000 + i}...")
        proc = subprocess.Popen(
            ['luajit', 'main.lua'],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )

        # Spam the button: Inject the Node ID
        proc.stdin.write(f"{i}\n")
        proc.stdin.flush()
        nodes.append(proc)

        threading.Thread(target=monitor_output, args=(proc, i), daemon=True).start()
        time.sleep(0.1) # Tiny delay just to ensure sequential stdout logging

    print("\n===========================================")
    print(" [HARNESS] 8-WAY MESH ALLOCATED AND POLLING")
    print("===========================================")
    print("1. All nodes cross-connected on 127.0.0.1:50000-50007")
    print("2. Deep History Packets actively circulating.")
    print("3. Press Ctrl+C to nuke the Multiverse.")
    print("===========================================\n")

    try:
        for node in nodes:
            node.wait()
    except KeyboardInterrupt:
        print("\n[HARNESS] Ctrl+C Detected. Nuking the Multiverse...")
        for node in nodes:
            node.terminate()
        sync_log.close()
        print("[HARNESS] Clean Exit.")

if __name__ == "__main__":
    main()
