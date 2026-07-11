// Producer/consumer smoke test using pthread_cond_t + pthread_mutex_t.
//
// Exercises mlibc's condvar path (thread_cond_broadcast/thread_cond_timedwait
// in mlibc/options/internal/generic/threads.cpp), which is ported but was
// never actually driven end-to-end on this kernel before this test — unlike
// the plain mutex-only pthread_test, this needs sys_futex_wait/wake to
// correctly handle *two* distinct futex words (the mutex's state and the
// condvar's sequence counter) interleaving across threads.
//
// One producer pushes ITEMS ints into a small ring buffer; one consumer
// drains it, blocking on the condvar whenever the buffer is empty. The
// producer signals after each push; the consumer signals after each pop
// (so the producer can unblock if it ever fills the buffer).

#include <stdio.h>
#include <pthread.h>

#define CAPACITY 4
#define ITEMS 20

static int buffer[CAPACITY];
static int head = 0, tail = 0, count = 0;
static int produced_sum = 0, consumed_sum = 0;
static int done = 0;

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;
static pthread_cond_t not_full = PTHREAD_COND_INITIALIZER;

static void *producer(void *arg) {
    (void)arg;
    for (int i = 1; i <= ITEMS; i++) {
        pthread_mutex_lock(&lock);
        while (count == CAPACITY) {
            pthread_cond_wait(&not_full, &lock);
        }
        buffer[tail] = i;
        tail = (tail + 1) % CAPACITY;
        count++;
        produced_sum += i;
        pthread_cond_broadcast(&not_empty);
        pthread_mutex_unlock(&lock);
    }

    pthread_mutex_lock(&lock);
    done = 1;
    pthread_cond_broadcast(&not_empty);
    pthread_mutex_unlock(&lock);

    printf("producer: done, produced_sum=%d\n", produced_sum);
    return 0;
}

static void *consumer(void *arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&lock);
        while (count == 0 && !done) {
            pthread_cond_wait(&not_empty, &lock);
        }
        if (count == 0 && done) {
            pthread_mutex_unlock(&lock);
            break;
        }
        int item = buffer[head];
        head = (head + 1) % CAPACITY;
        count--;
        consumed_sum += item;
        pthread_cond_broadcast(&not_full);
        pthread_mutex_unlock(&lock);
    }

    printf("consumer: done, consumed_sum=%d\n", consumed_sum);
    return 0;
}

int main(void) {
    printf("producer_consumer: starting (capacity=%d, items=%d)\n", CAPACITY, ITEMS);

    pthread_t prod_thread, cons_thread;
    if (pthread_create(&cons_thread, NULL, consumer, NULL) != 0) {
        printf("producer_consumer: pthread_create(consumer) failed\n");
        return 1;
    }
    if (pthread_create(&prod_thread, NULL, producer, NULL) != 0) {
        printf("producer_consumer: pthread_create(producer) failed\n");
        return 1;
    }

    pthread_join(prod_thread, NULL);
    pthread_join(cons_thread, NULL);

    int expected = ITEMS * (ITEMS + 1) / 2;
    printf("producer_consumer: produced_sum=%d consumed_sum=%d expected=%d\n",
           produced_sum, consumed_sum, expected);

    if (produced_sum == expected && consumed_sum == expected) {
        printf("producer_consumer: PASS\n");
    } else {
        printf("producer_consumer: FAIL\n");
    }

    return 0;
}
