import os
import time

from mq_bridge import Message, Publisher, Route


def handle_order(message: Message) -> dict:
    print("id:", message.id)
    print("metadata:", message.metadata)
    print("payload:", message.payload)
    return {"handled": True}


CONFIG_PATH = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "examples",
    "memory.yaml",
)


def main() -> None:
    route = Route.from_file(CONFIG_PATH, "orders_route").with_handler(handle_order)
    publisher = Publisher.from_file(CONFIG_PATH, "orders_publisher")

    # start() deploys the route on a background thread and returns, so the rest
    # of this function keeps running. (Use route.run() instead when you want the
    # call to block until another thread stops the route.)
    route.start()
    try:
        publisher.send_json(
            {"order_id": 42, "status": "created"},
            {"kind": "order.created"},
        )
        time.sleep(0.2)  # let the route process the message before we stop
    finally:
        route.stop()
        route.join()


if __name__ == "__main__":
    main()
