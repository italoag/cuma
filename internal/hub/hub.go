package hub

import (
	"encoding/json"
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

type EventType string

const (
	EventDeviceDiscovered EventType = "device.discovered"
	EventDeviceUpdated    EventType = "device.updated"
	EventDeviceOffline    EventType = "device.offline"
	EventScanStarted      EventType = "scan.started"
	EventScanProgress     EventType = "scan.progress"
	EventScanCompleted    EventType = "scan.completed"
	EventScanFailed       EventType = "scan.failed"
)

type Event struct {
	Type      EventType   `json:"type"`
	Timestamp time.Time   `json:"timestamp"`
	Payload   interface{} `json:"payload"`
}

type Client struct {
	conn *websocket.Conn
	send chan []byte
}

type Hub struct {
	clients    map[*Client]bool
	broadcast  chan Event
	register   chan *Client
	unregister chan *Client
	mu         sync.RWMutex
}

func New() *Hub {
	return &Hub{
		clients:    make(map[*Client]bool),
		broadcast:  make(chan Event, 256),
		register:   make(chan *Client, 16),
		unregister: make(chan *Client, 16),
	}
}

func (h *Hub) Run() {
	for {
		select {
		case client := <-h.register:
			h.mu.Lock()
			h.clients[client] = true
			h.mu.Unlock()

		case client := <-h.unregister:
			h.mu.Lock()
			if _, ok := h.clients[client]; ok {
				delete(h.clients, client)
				close(client.send)
			}
			h.mu.Unlock()

		case event := <-h.broadcast:
			payload, err := json.Marshal(event)
			if err != nil {
				continue
			}
			h.mu.RLock()
			for client := range h.clients {
				select {
				case client.send <- payload:
				default:
					// slow client: disconnect
					go func(c *Client) { h.unregister <- c }(client)
				}
			}
			h.mu.RUnlock()
		}
	}
}

func (h *Hub) Broadcast(eventType EventType, payload interface{}) {
	h.broadcast <- Event{
		Type:      eventType,
		Timestamp: time.Now().UTC(),
		Payload:   payload,
	}
}

// RegisterClient registers a WebSocket connection and starts read/write pumps.
// It returns a done channel that closes when the client disconnects.
func (h *Hub) RegisterClient(conn *websocket.Conn) <-chan struct{} {
	client := &Client{
		conn: conn,
		send: make(chan []byte, 256),
	}
	h.register <- client
	done := make(chan struct{})

	go client.writePump(h, done)
	go client.readPump(h)

	return done
}

const (
	writeWait  = 10 * time.Second
	pingPeriod = 30 * time.Second
	maxMsgSize = 512
)

func (c *Client) writePump(h *Hub, done chan struct{}) {
	ticker := time.NewTicker(pingPeriod)
	defer func() {
		ticker.Stop()
		c.conn.Close()
		close(done)
	}()

	for {
		select {
		case msg, ok := <-c.send:
			c.conn.SetWriteDeadline(time.Now().Add(writeWait))
			if !ok {
				c.conn.WriteMessage(websocket.CloseMessage, []byte{})
				return
			}
			if err := c.conn.WriteMessage(websocket.TextMessage, msg); err != nil {
				return
			}

		case <-ticker.C:
			c.conn.SetWriteDeadline(time.Now().Add(writeWait))
			if err := c.conn.WriteMessage(websocket.PingMessage, nil); err != nil {
				return
			}
		}
	}
}

func (c *Client) readPump(h *Hub) {
	defer func() { h.unregister <- c }()
	c.conn.SetReadLimit(maxMsgSize)
	c.conn.SetReadDeadline(time.Now().Add(60 * time.Second))
	c.conn.SetPongHandler(func(string) error {
		c.conn.SetReadDeadline(time.Now().Add(60 * time.Second))
		return nil
	})
	for {
		if _, _, err := c.conn.ReadMessage(); err != nil {
			return
		}
	}
}
