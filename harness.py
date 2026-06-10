import subprocess
import threading
import re
import sys
import time
import os

# FIXED: Match exactly what proxy.lua prints -> "[LOBBY] Hosted! Code: [XXXX]"
CODE_PATTERN = re.compile(r"Code:\s*\[([A-Za-z0-9]+)\]")

# Master sync log for unified timeline debugging
sync_log = open("multiverse_sync.log", "w")
sync_lock = threading.Lock()

def monitor_output(process, p_id):
    """Reads stdout from a child process, logs everything, and filters heartbeats/toggles."""
    with open(f"p{p_id}_full.log", "w") as full_log:
        # iter() reads line-by-line in real-time until process terminates
        for line in iter(process.stdout.readline, ''):
            full_log.write(line)
            full_log.flush()

            # Filter for the specific events you want to track
            if "[HEARTBEAT]" in line or "Toggle ->" in line:
                with sync_lock:
                    formatted_line = f"[P{p_id}] {line}"
                    sys.stdout.write(formatted_line) # Echo to terminal
                    sync_log.write(formatted_line)   # Write to master timeline
                    sync_log.flush()

def main():
    print("===========================================")
    print(" IGNITING MULTIVERSE TEST HARNESS (8 NODES)")
    print("===========================================")

    # 1. Spawn the Host
    print("[HARNESS] Launching Host (P0)...")
    host_proc = subprocess.Popen(
        ['luajit','main.lua'],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1
    )

    # Inject '1' to select Host mode
    host_proc.stdin.write("1\n")
    host_proc.stdin.flush()

    lobby_code = None

    # Read Host output synchronously until we secure the lobby code
    with open("p0_full.log", "w") as f:
        while True:
            line = host_proc.stdout.readline()
            if not line: break

            f.write(line)
            f.flush()
            sys.stdout.write(f"[P0_INIT] {line}")

            match = CODE_PATTERN.search(line)
            if match:
                lobby_code = match.group(1)
                break

    if not lobby_code:
        print("[HARNESS] FATAL: Host died or failed to generate Lobby Code.")
        sys.exit(1)

    print(f"\n[HARNESS] Secured Lobby Code: [{lobby_code}]. Deploying 7 Guests...\n")

    # Hand off the rest of the Host's stdout to an async background thread
    threading.Thread(target=monitor_output, args=(host_proc, 0), daemon=True).start()

    guests = []

    # 2. Spawn 7 Guests
    for i in range(1, 8):
        time.sleep(1.1) # Optional: Uncomment to stagger launches if Matchmaker chokes
        guest_proc = subprocess.Popen(
            ['luajit','main.lua'],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )
        # Inject '2' for join, then inject the code
        guest_proc.stdin.write("2\n")
        guest_proc.stdin.flush()
        guest_proc.stdin.write(f"{lobby_code}\n")
        guest_proc.stdin.flush()

        guests.append(guest_proc)

        # Start async monitor thread for this guest
        threading.Thread(target=monitor_output, args=(guest_proc, i), daemon=True).start()

    print("\n===========================================")
    print(" [HARNESS] ALL NODES ALLOCATED AND POLLING")
    print("===========================================")
    print("1. Matchmaker will automatically lock the mesh when 8 nodes join.")
    print("2. Holding cell and T-Minus countdown will begin automatically.")
    print("3. Press Ctrl+C in this terminal to stop the whole cluster.")
    print("4. Live timeline compiling to: multiverse_sync.log")
    print("===========================================\n")

    try:
        # Keep main thread alive while children run
        host_proc.wait()
        for g in guests: g.wait()
    except KeyboardInterrupt:
        print("\n[HARNESS] Ctrl+C Detected. Nuking the Multiverse...")
        host_proc.terminate()
        for g in guests: g.terminate()
        sync_log.close()
        print("[HARNESS] Clean Exit.")

if __name__ == "__main__":
    main()
