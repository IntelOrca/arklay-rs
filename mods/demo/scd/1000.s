.version 1

.init
.block
    set                     FG_SCENARIO, 1, 0

.main
.block
    message                 0, 0
    evt_exec                0, 0, event_00

.event event_00
    evt_single              6
    set                     FG_SCENARIO, 3, 0
    evt_single              4
    task_kill               1
    evt_single              4
    remove_item             ITEM_CLIP
    evt_single              6
    swap_var                0, 0, 12
    evt_finish
