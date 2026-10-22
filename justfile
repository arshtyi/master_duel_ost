game_dir := '/mnt/c/Software/Steam/steamapps/common/Yu-Gi-Oh!  Master Duel'

default:
    @just --list

extract input=game_dir:
    cargo run --release -- extract '{{ input }}'

render *args:
    cargo run --release -- render {{ args }}
