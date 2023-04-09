import json
import sys


def parse_log(line):
    label_index = line.index("[flood_publishing_test]")
    log_str = line[label_index + len("[flood_publishing_test]"):]
    log = json.loads(log_str)
    log['node_id'] = extract_node_id(line)
    return log


# Extract `node_id`, issued by Testground from a log.
# For example, this function returns `a04ed9` if a log like below is passed to:
# Mar 31 21:52:47.918675  INFO    0.8177s      OTHER << single[002] (a04ed9) >> [flood_publishing_test]...
def extract_node_id(line):
    label_sep = line.index(") >>")
    return line[label_sep - 6:label_sep]


def node_id_to_peer_id(nodes, node_id):
    for key, node in nodes.items():
        if key == node_id:
            return node['peer_id']

    print("node_id not found in nodes: " + node_id)
    sys.exit(1)


def mean(latencies):
    return sum(latencies) / len(latencies)


def median(latencies):
    sorted_latencies = sorted(latencies)
    length = len(sorted_latencies)
    if length == 0:
        return None
    elif length % 2 == 0:
        return (sorted_latencies[length // 2 - 1] + sorted_latencies[length // 2]) / 2
    else:
        return sorted_latencies[length // 2]


# Example logs for debugging.
debug = '''Mar 31 21:57:30.155618  INFO    0.8585s      OTHER << single[000] (3b20a6) >> [flood_publishing_test]{"event":"peer_id","peer_id":"12D3KooWN5f2YP3sKRykhAYEz48cfpGseW9dRfJxWDKXmKbb22c1","is_publisher":false}
Mar 31 21:57:30.157546  INFO    0.8605s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"peer_id","peer_id":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","is_publisher":true}
Mar 31 21:57:30.159084  INFO    0.8620s      OTHER << single[002] (591350) >> [flood_publishing_test]{"event":"peer_id","peer_id":"12D3KooWAHNzQSmfpCF4r7WT8G7tWQFc2Yr6wES3Poz99r3RbkAy","is_publisher":false}
Mar 31 21:57:36.156824  INFO    6.8598s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWN5f2YP3sKRykhAYEz48cfpGseW9dRfJxWDKXmKbb22c1","message_id":"c189ad462fb14701d0b1040139e119c60f3172b2","time":1680299856156}
Mar 31 21:57:36.156965  INFO    6.8599s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWAHNzQSmfpCF4r7WT8G7tWQFc2Yr6wES3Poz99r3RbkAy","message_id":"c189ad462fb14701d0b1040139e119c60f3172b2","time":1680299856156}
Mar 31 21:57:36.862880  INFO    7.5658s      OTHER << single[002] (591350) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"c189ad462fb14701d0b1040139e119c60f3172b2","time":1680299856862}
Mar 31 21:57:36.928091  INFO    7.6310s      OTHER << single[000] (3b20a6) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"c189ad462fb14701d0b1040139e119c60f3172b2","time":1680299856927}
Mar 31 21:57:39.166998  INFO    9.8700s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWN5f2YP3sKRykhAYEz48cfpGseW9dRfJxWDKXmKbb22c1","message_id":"25b377b39fd58a9b57f3a7c8fea759063fcfcc1a","time":1680299859158}
Mar 31 21:57:39.167076  INFO    9.8700s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWAHNzQSmfpCF4r7WT8G7tWQFc2Yr6wES3Poz99r3RbkAy","message_id":"25b377b39fd58a9b57f3a7c8fea759063fcfcc1a","time":1680299859158}
Mar 31 21:57:39.899528  INFO    10.6025s      OTHER << single[000] (3b20a6) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"25b377b39fd58a9b57f3a7c8fea759063fcfcc1a","time":1680299859899}
Mar 31 21:57:39.941150  INFO    10.6441s      OTHER << single[002] (591350) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"25b377b39fd58a9b57f3a7c8fea759063fcfcc1a","time":1680299859940}
Mar 31 21:57:42.156844  INFO    12.8598s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWAHNzQSmfpCF4r7WT8G7tWQFc2Yr6wES3Poz99r3RbkAy","message_id":"472b99c805f1d58a2155d4bf654cbcb58d12b75b","time":1680299862155}
Mar 31 21:57:42.157266  INFO    12.8602s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWN5f2YP3sKRykhAYEz48cfpGseW9dRfJxWDKXmKbb22c1","message_id":"472b99c805f1d58a2155d4bf654cbcb58d12b75b","time":1680299862156}
Mar 31 21:57:42.873985  INFO    13.5770s      OTHER << single[002] (591350) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"472b99c805f1d58a2155d4bf654cbcb58d12b75b","time":1680299862873}
Mar 31 21:57:42.927309  INFO    13.6303s      OTHER << single[000] (3b20a6) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"472b99c805f1d58a2155d4bf654cbcb58d12b75b","time":1680299862926}
Mar 31 21:57:45.156938  INFO    15.8599s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWAHNzQSmfpCF4r7WT8G7tWQFc2Yr6wES3Poz99r3RbkAy","message_id":"4ddbd9ff71a5ceebb8b1f3d4353a217594e11439","time":1680299865156}
Mar 31 21:57:45.157038  INFO    15.8600s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWN5f2YP3sKRykhAYEz48cfpGseW9dRfJxWDKXmKbb22c1","message_id":"4ddbd9ff71a5ceebb8b1f3d4353a217594e11439","time":1680299865156}
Mar 31 21:57:45.858130  INFO    16.5611s      OTHER << single[002] (591350) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"4ddbd9ff71a5ceebb8b1f3d4353a217594e11439","time":1680299865851}
Mar 31 21:57:45.952577  INFO    16.6556s      OTHER << single[000] (3b20a6) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"4ddbd9ff71a5ceebb8b1f3d4353a217594e11439","time":1680299865928}
Mar 31 21:57:48.157564  INFO    18.8606s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWAHNzQSmfpCF4r7WT8G7tWQFc2Yr6wES3Poz99r3RbkAy","message_id":"864403bdfd935b72df14701adf7fc44829c47357","time":1680299868156}
Mar 31 21:57:48.157612  INFO    18.8606s      OTHER << single[001] (a250a4) >> [flood_publishing_test]{"event":"send","to":"12D3KooWN5f2YP3sKRykhAYEz48cfpGseW9dRfJxWDKXmKbb22c1","message_id":"864403bdfd935b72df14701adf7fc44829c47357","time":1680299868156}
Mar 31 21:57:48.863088  INFO    19.5661s      OTHER << single[002] (591350) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"864403bdfd935b72df14701adf7fc44829c47357","time":1680299868862}
Mar 31 21:57:48.927562  INFO    19.6306s      OTHER << single[000] (3b20a6) >> [flood_publishing_test]{"event":"receive","propagation_source":"12D3KooWPf6ZJrYn1ojsp9iazfxJFRgYoHRC79yNDUviqSeS894x","message_id":"864403bdfd935b72df14701adf7fc44829c47357","time":1680299868927}
'''


def run():
    publisher = None
    nodes = {}

    # The logs of events that the `publisher` sent a message.
    send_logs = []
    # The logs of events that the `nodes` received a message from the `publisher`.
    receive_logs = {}

    # for line in debug.split('\n'):  # debugging
    for line in sys.stdin:
        if len(line) == 0:
            continue

        log = parse_log(line.rstrip())
        if log['event'] == 'peer_id':
            if log['is_publisher']:
                publisher = log
            else:
                nodes[log['node_id']] = log
        elif log['event'] == 'send':
            if log['node_id'] == publisher['node_id']:
                send_logs.append(log)
        elif log['event'] == 'receive':
            if log['node_id'] != publisher['node_id'] and log['propagation_source'] == publisher['peer_id']:
                key = node_id_to_peer_id(nodes, log['node_id']) + '_' + log['message_id']
                receive_logs[key] = log
        else:
            print("Unknown log: ", log)
            sys.exit(1)

    print("\n\n*** measure_latency.py ***")
    print('[publisher] node_id:', publisher['node_id'], ", peer_id:", publisher['peer_id'])
    print('[nodes]', len(nodes) + 1)  # nodes + publisher
    print('[send_logs]', len(send_logs))

    latencies = []

    for send in send_logs:
        receive = receive_logs.get(send['to'] + '_' + send['message_id'])
        # It's possible that a log may not be found even in normal cases, due to factors such as latency.
        if receive is not None:
            latencies.append(receive['time'] - send['time'])
    print('[receive_logs]', len(latencies))

    print('\n* Results (in milliseconds) *')
    print('[mean]', mean(latencies))
    print('[median]', median(latencies))


if __name__ == '__main__':
    run()
