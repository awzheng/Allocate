// allocate-cloud/main.go
//
// Allocate Cloud – Telemetry Ingestion Microservice (Phase 1 scaffold)
//
// Phase 2 will receive JSON payloads from allocate-core over a local Unix
// domain socket or lightweight NATS subject, persist them into InfluxDB /
// SQLite, and expose a read-side HTTP API for the SwiftUI dashboard.
//
// Architecture (planned):
//
//   allocate-core  ──gRPC/UDS──►  allocate-cloud  ──►  InfluxDB
//                                       │
//                                    HTTP :8080
//                                       │
//                                  allocate-ui (dashboard)
package main

import (
	"encoding/json"
	"log"
	"net/http"
	"time"
)

// SnapshotPayload mirrors the JSON that allocate-core will eventually POST.
// Fields align with the CpuSnapshot type in process.rs.
type SnapshotPayload struct {
	Timestamp   time.Time         `json:"ts"`
	ForegroundApp string          `json:"fg_app"`
	ForegroundPID int             `json:"fg_pid"`
	TopHogs     []ProcessHog      `json:"top_hogs"`
}

// ProcessHog is one entry from the top-N background CPU consumers list.
type ProcessHog struct {
	Name   string  `json:"name"`
	CpuPct float64 `json:"cpu_pct"`
}

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("/telemetry", handleTelemetry)
	mux.HandleFunc("/health",    handleHealth)

	addr := ":8080"
	log.Printf("[allocate-cloud] Telemetry ingestion service listening on %s", addr)

	server := &http.Server{
		Addr:         addr,
		Handler:      mux,
		ReadTimeout:  5 * time.Second,
		WriteTimeout: 5 * time.Second,
	}

	if err := server.ListenAndServe(); err != nil {
		log.Fatalf("[allocate-cloud] Fatal: %v", err)
	}
}

// handleTelemetry accepts JSON snapshots from allocate-core.
// Phase 1: echoes the payload back (no persistence yet).
// Phase 2: will write to InfluxDB / SQLite.
func handleTelemetry(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	var payload SnapshotPayload
	if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
		http.Error(w, "bad request: "+err.Error(), http.StatusBadRequest)
		return
	}

	log.Printf("[telemetry] fg=%s pid=%d hogs=%d",
		payload.ForegroundApp, payload.ForegroundPID, len(payload.TopHogs))

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusAccepted)
	_ = json.NewEncoder(w).Encode(map[string]string{"status": "accepted"})
}

// handleHealth returns a simple liveness probe (for Docker / launchctl).
func handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]string{
		"status":  "healthy",
		"service": "allocate-cloud",
	})
}
