// A tether::Server's I/O over a UART, on ESP-IDF: the glue between the
// sans-IO core and FreeRTOS.
//
// One task, the I/O task, calls serve(): it feeds what the UART receives to
// the server, writes what the server has to send, and sleeps until bytes
// arrive, the link's next deadline, or another task queues something. The
// server's handlers run there, holding the server's lock: they mustn't block,
// and they answer at once or keep their reply or sink for later.
//
// Any task can use replies and sinks: UartIo is the server's ServerHooks, a
// recursive mutex and the wake-up for the I/O task. When the send queue is
// full or a channel has no credit, a task that can wait calls wait(), which
// retries until it goes through (every time something arrives, which is what
// frees the queue and brings credit), fails for good, or times out.
//
//   tether::idf::UartIo io({.port = UART_NUM_1, .tx_pin = 17, .rx_pin = 16});
//   for (;;) {  // In the I/O task.
//     server.restart(esp_random() | 1);
//     io.serve(server);
//   }
//
//   // In another task:
//   const auto sent = io.wait([&] {
//     return sink.send([&](flatbuffers::FlatBufferBuilder& fbb) { ... });
//   }, 1s);
//
// The UART driver and a FreeRTOS queue set come from the heap, once, when
// UartIo is made; nothing else does.

#pragma once

#include <array>
#include <concepts>
#include <cstddef>
#include <cstdint>
#include <expected>
#include <type_traits>

#include "driver/uart.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "tether/function_ref.h"
#include "tether/server.h"

namespace tether::idf {

struct UartConfig {
  uart_port_t port = UART_NUM_1;
  int tx_pin = UART_PIN_NO_CHANGE;
  int rx_pin = UART_PIN_NO_CHANGE;
  // The line's rate: the same as the server's LinkConfig, and the peer's.
  uint32_t baud_rate = 921600;
  // The driver's buffers. What arrives while the I/O task is busy waits in
  // the receive one; past that, in the hardware FIFO, and past that it's lost
  // (and retransmitted).
  int rx_buffer_size = 4096;
  int tx_buffer_size = 4096;
  int event_queue_size = 16;
  // Bytes in the hardware FIFO (128 on the ESP32) before the driver's
  // interrupt empties it. What's left over is the time the interrupt may be
  // late before bytes are lost: 64 bytes is ~430 µs at 1.5 Mbaud, where
  // ESP-IDF's default of 120 leaves 8 bytes, ~50 µs.
  int rx_full_threshold = 64;
};

struct UartStats {
  // Bytes were lost: the hardware FIFO filled up before the driver's interrupt
  // emptied it (it was late, or the receive buffer was full). The frame they
  // belonged to fails its CRC and is retransmitted; the rest is kept.
  uint32_t overflows = 0;
  // The driver's receive buffer filled up, as the I/O task fell behind: the
  // driver stops taking bytes from the FIFO until it reads. Nothing is lost
  // unless the FIFO overflows meanwhile.
  uint32_t buffer_full = 0;
  // Framing, parity and break errors on the line.
  uint32_t line_errors = 0;
};

class UartIo final : private ServerHooks {
 public:
  // Installs the UART's driver: aborts if it can't, as a wiring bug.
  explicit UartIo(const UartConfig& config);
  ~UartIo();
  UartIo(const UartIo&) = delete;
  UartIo& operator=(const UartIo&) = delete;

  // Runs `server`'s I/O on the calling task until its link ends for good, and
  // returns how it ended. `tick` runs on this task after each read, which is
  // when acks free the send queue and credit arrives: for apps that send from
  // the I/O task instead of waiting in their own.
  LinkState serve(Server& server);
  LinkState serve(Server& server, FunctionRef<void()> tick);

  // Calls `send` until it isn't QueueFull or NoCredit, retrying each time
  // something arrives, for up to `timeout`; returns its last result. `send`
  // runs holding the server's lock. Not from the I/O task, nor holding the
  // lock (in a build, or a handler): either aborts, as it would deadlock.
  template <typename Send>
    requires std::same_as<std::invoke_result_t<Send&>,
                          std::expected<void, CallError>>
  std::expected<void, CallError> wait(Send&& send, Millis timeout) {
    return wait_for(FunctionRef<std::expected<void, CallError>()>(send),
                    timeout);
  }

  // Updated by the I/O task.
  [[nodiscard]] const UartStats& stats() const { return stats_; }

 private:
  // A task in wait(), in a list on the waiting tasks' stacks.
  struct Waiter {
    SemaphoreHandle_t woken;
    Waiter* next = nullptr;
  };

  void lock() override;
  void unlock() override;
  void wake() override;

  std::expected<void, CallError> wait_for(
      FunctionRef<std::expected<void, CallError>()> send, Millis timeout);
  // Wakes every task in wait().
  void wake_waiters();
  void remove(Waiter& waiter);
  // Feeds the server what the driver has buffered; whether there was any.
  bool read_all(Server& server);
  void handle_event();

  uart_port_t port_;
  QueueHandle_t events_ = nullptr;
  QueueSetHandle_t set_ = nullptr;
  StaticSemaphore_t mutex_memory_{};
  SemaphoreHandle_t mutex_;
  StaticSemaphore_t wake_memory_{};
  SemaphoreHandle_t wake_;
  // The task in serve(), if any.
  TaskHandle_t io_task_ = nullptr;
  // Under the lock.
  Waiter* waiters_ = nullptr;
  UartStats stats_{};
  std::array<std::byte, 256> rx_{};
};

}  // namespace tether::idf
