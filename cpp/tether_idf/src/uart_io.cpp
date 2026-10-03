#include "tether_idf/uart_io.h"

#include <algorithm>
#include <cstdlib>
#include <utility>

#include "esp_check.h"
#include "esp_log.h"
#include "esp_timer.h"

namespace tether::idf {
namespace {

constexpr char kTag[] = "tether_idf";

Millis now() { return Millis(esp_timer_get_time() / 1000); }

// Ticks to wait for `duration`, rounded up: a deadline isn't met early.
TickType_t ticks(Millis duration) {
  if (duration <= Millis(0)) {
    return 0;
  }
  const uint64_t ticks =
      (static_cast<uint64_t>(duration.count()) * configTICK_RATE_HZ + 999) /
      1000;
  return static_cast<TickType_t>(std::min<uint64_t>(ticks, portMAX_DELAY - 1));
}

}  // namespace

UartIo::UartIo(const UartConfig& config)
    : port_(config.port),
      mutex_(xSemaphoreCreateRecursiveMutexStatic(&mutex_memory_)),
      wake_(xSemaphoreCreateBinaryStatic(&wake_memory_)) {
  ESP_ERROR_CHECK(uart_driver_install(port_, config.rx_buffer_size,
                                      config.tx_buffer_size,
                                      config.event_queue_size, &events_, 0));
  // Before the pins are connected, while the driver's queue is still empty,
  // as a queue joins a set only empty.
  set_ = xQueueCreateSet(config.event_queue_size + 1);
  if (set_ == nullptr || xQueueAddToSet(events_, set_) != pdPASS ||
      xQueueAddToSet(wake_, set_) != pdPASS) {
    ESP_LOGE(kTag, "can't make the queue set");
    std::abort();
  }
  const uart_config_t uart{
      .baud_rate = static_cast<int>(config.baud_rate),
      .data_bits = UART_DATA_8_BITS,
      .parity = UART_PARITY_DISABLE,
      .stop_bits = UART_STOP_BITS_1,
      .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
      .rx_flow_ctrl_thresh = 0,
      .rx_glitch_filt_thresh = 0,
      .source_clk = UART_SCLK_DEFAULT,
      .flags = {},
  };
  ESP_ERROR_CHECK(uart_param_config(port_, &uart));
  ESP_ERROR_CHECK(uart_set_pin(port_, config.tx_pin, config.rx_pin,
                               UART_PIN_NO_CHANGE, UART_PIN_NO_CHANGE));
}

UartIo::~UartIo() {
  // Out of the set first, as a queue can't be deleted while it's in one; and
  // only empty can it leave.
  uart_disable_rx_intr(port_);
  xSemaphoreTake(wake_, 0);
  xQueueReset(events_);
  xQueueRemoveFromSet(wake_, set_);
  xQueueRemoveFromSet(events_, set_);
  vQueueDelete(set_);
  uart_driver_delete(port_);
  vSemaphoreDelete(wake_);
  vSemaphoreDelete(mutex_);
}

LinkState UartIo::serve(Server& server) {
  return serve(server, [] {});
}

LinkState UartIo::serve(Server& server, FunctionRef<void()> tick) {
  server.set_hooks(*this);
  io_task_ = xTaskGetCurrentTaskHandle();
  for (;;) {
    while (const auto wire = server.poll_transmit(now())) {
      uart_write_bytes(port_, wire->data(), wire->size());
    }
    if (is_terminal(server.link().state())) {
      break;
    }
    const auto deadline = server.next_deadline();
    const TickType_t wait = deadline ? ticks(*deadline - now()) : portMAX_DELAY;
    const QueueSetMemberHandle_t woken = xQueueSelectFromSet(set_, wait);
    if (woken == wake_) {
      xSemaphoreTake(wake_, 0);
    } else if (woken == events_) {
      handle_event();
    }
    // Whatever woke us, read what's there.
    if (read_all(server)) {
      tick();
      wake_waiters();
    }
  }
  io_task_ = nullptr;
  // Their calls are closed now: let them see it.
  wake_waiters();
  return server.link().state();
}

std::expected<void, CallError> UartIo::wait_for(
    FunctionRef<std::expected<void, CallError>()> send, Millis timeout) {
  const TaskHandle_t self = xTaskGetCurrentTaskHandle();
  if (self == io_task_ || xSemaphoreGetMutexHolder(mutex_) == self) {
    ESP_LOGE(kTag, "wait() would deadlock: %s",
             self == io_task_ ? "it's the I/O task" : "it holds the lock");
    std::abort();
  }
  StaticSemaphore_t woken_memory;
  Waiter waiter{.woken = xSemaphoreCreateBinaryStatic(&woken_memory)};
  const TickType_t start = xTaskGetTickCount();
  const TickType_t limit = ticks(timeout);
  std::expected<void, CallError> result;
  for (;;) {
    lock();
    result = send();
    const bool retry = !result && (result.error() == CallError::QueueFull ||
                                   result.error() == CallError::NoCredit);
    if (retry) {
      // Under the lock, so nothing that arrives meanwhile is missed.
      waiter.next = waiters_;
      waiters_ = &waiter;
    }
    unlock();
    if (!retry) {
      break;
    }
    const TickType_t elapsed = xTaskGetTickCount() - start;
    const bool woken = elapsed < limit &&
                       xSemaphoreTake(waiter.woken, limit - elapsed) == pdTRUE;
    if (!woken) {
      lock();
      remove(waiter);
      unlock();
      break;
    }
  }
  vSemaphoreDelete(waiter.woken);
  return result;
}

void UartIo::wake_waiters() {
  lock();
  for (Waiter* w = std::exchange(waiters_, nullptr); w != nullptr;) {
    Waiter* const next = std::exchange(w->next, nullptr);
    xSemaphoreGive(w->woken);
    w = next;
  }
  unlock();
}

void UartIo::remove(Waiter& waiter) {
  for (Waiter** at = &waiters_; *at != nullptr; at = &(*at)->next) {
    if (*at == &waiter) {
      *at = waiter.next;
      return;
    }
  }
}

bool UartIo::read_all(Server& server) {
  bool any = false;
  for (;;) {
    std::size_t buffered = 0;
    ESP_ERROR_CHECK(uart_get_buffered_data_len(port_, &buffered));
    if (buffered == 0) {
      return any;
    }
    const int n =
        uart_read_bytes(port_, rx_.data(), std::min(buffered, rx_.size()), 0);
    if (n <= 0) {
      return any;
    }
    server.receive(std::span(rx_).first(static_cast<std::size_t>(n)), now());
    any = true;
  }
}

void UartIo::handle_event() {
  uart_event_t event;
  if (xQueueReceive(events_, &event, 0) != pdTRUE) {
    return;
  }
  switch (event.type) {
    case UART_FIFO_OVF:
    case UART_BUFFER_FULL:
      // What's buffered is cut short somewhere; the link retransmits it.
      ++stats_.overflows;
      ESP_LOGW(kTag, "receive overflow: bytes lost");
      uart_flush_input(port_);
      break;
    case UART_BREAK:
    case UART_PARITY_ERR:
    case UART_FRAME_ERR:
      ++stats_.line_errors;
      break;
    default:
      break;
  }
}

void UartIo::lock() { xSemaphoreTakeRecursive(mutex_, portMAX_DELAY); }

void UartIo::unlock() { xSemaphoreGiveRecursive(mutex_); }

void UartIo::wake() { xSemaphoreGive(wake_); }

}  // namespace tether::idf
